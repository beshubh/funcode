mod edit;
mod read;
mod search;
mod terminal;
mod workspace;

pub(crate) use workspace::{ReadFile, WorkspaceFileReader};

use crate::composer::SessionMode;
use futures::future::BoxFuture;
use serde_json::Value;
use std::{
    fmt,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};
use tokio::sync::mpsc;

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ToolSpec {
    pub(crate) name: &'static str,
    pub(crate) description: String,
    pub(crate) parameters: Value,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ToolDisplay {
    CodeRange {
        path: String,
        start_line: u32,
        end_line: u32,
        content: String,
    },
    SearchResults {
        query: String,
        matches: String,
    },
    Patch {
        path: String,
        diff: String,
    },
    Terminal {
        description: String,
        command: String,
        output: String,
        exit_code: Option<i32>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ToolInvocation {
    pub(crate) summary: String,
    pub(crate) display: Option<ToolDisplay>,
}

impl ToolInvocation {
    fn running(summary: impl Into<String>) -> Self {
        Self {
            summary: summary.into(),
            display: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ToolResult {
    pub(crate) output: String,
    pub(crate) display: ToolDisplay,
    pub(crate) summary: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ToolEvent {
    Started {
        call_id: u64,
        name: String,
        summary: String,
        display: Option<ToolDisplay>,
    },
    OutputDelta {
        call_id: u64,
        chunk: String,
    },
    Finished {
        call_id: u64,
        summary: Option<String>,
        display: ToolDisplay,
    },
    Failed {
        call_id: u64,
        message: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ToolFailureKind {
    Domain,
    Infrastructure,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ToolFailure {
    message: String,
    kind: ToolFailureKind,
}

impl ToolFailure {
    pub(crate) fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            kind: ToolFailureKind::Domain,
        }
    }

    pub(crate) fn infrastructure(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            kind: ToolFailureKind::Infrastructure,
        }
    }

    fn is_infrastructure(&self) -> bool {
        self.kind == ToolFailureKind::Infrastructure
    }
}

impl fmt::Display for ToolFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ToolFailure {}

pub(crate) trait AgentTool: Send + Sync {
    fn spec(&self, mode: SessionMode) -> ToolSpec;

    fn availability(&self) -> ToolAvailability {
        ToolAvailability::AllModes
    }

    fn invocation(&self, arguments: &str) -> ToolInvocation {
        let _ = arguments;
        ToolInvocation::running(format!("Running {}", self.spec(SessionMode::Build).name))
    }

    fn execute(
        &self,
        arguments: String,
        context: ToolExecutionContext,
    ) -> BoxFuture<'static, Result<ToolResult, ToolFailure>>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ToolAvailability {
    AllModes,
    BuildOnly,
}

impl ToolAvailability {
    fn includes(self, mode: SessionMode) -> bool {
        self == Self::AllModes || mode == SessionMode::Build
    }
}

#[derive(Clone)]
pub(crate) struct ToolExecutionContext {
    workspace: Arc<workspace::Workspace>,
    events: mpsc::UnboundedSender<ToolEvent>,
    call_id: u64,
}

impl ToolExecutionContext {
    pub(crate) fn workspace(&self) -> &workspace::Workspace {
        &self.workspace
    }

    pub(crate) fn output(&self, chunk: impl Into<String>) {
        let _ = self.events.send(ToolEvent::OutputDelta {
            call_id: self.call_id,
            chunk: chunk.into(),
        });
    }
}

#[derive(Clone)]
pub(crate) struct ToolSession {
    mode: SessionMode,
    registry: ToolRegistry,
    workspace: Arc<workspace::Workspace>,
    events: mpsc::UnboundedSender<ToolEvent>,
    next_call_id: Arc<AtomicU64>,
}

impl ToolSession {
    pub(crate) fn new(
        root: PathBuf,
        mode: SessionMode,
        events: mpsc::UnboundedSender<ToolEvent>,
        request_id: u64,
    ) -> Result<Self, ToolFailure> {
        Ok(Self {
            mode,
            registry: ToolRegistry::for_mode(mode),
            workspace: Arc::new(workspace::Workspace::new(root)?),
            events,
            next_call_id: Arc::new(AtomicU64::new(request_id.wrapping_shl(32) | 0x8000_0000)),
        })
    }

    pub(crate) fn tools(&self) -> Vec<Arc<dyn AgentTool>> {
        self.registry.tools.clone()
    }

    pub(crate) fn spec(&self, tool: &dyn AgentTool) -> ToolSpec {
        tool.spec(self.mode)
    }

    pub(crate) async fn execute(
        &self,
        tool: Arc<dyn AgentTool>,
        arguments: String,
    ) -> Result<String, ToolFailure> {
        let call_id = self.next_call_id.fetch_add(1, Ordering::Relaxed);
        let invocation = tool.invocation(&arguments);
        let name = tool.spec(self.mode).name;
        let _ = self.events.send(ToolEvent::Started {
            call_id,
            name: name.to_owned(),
            summary: invocation.summary,
            display: invocation.display,
        });
        let context = ToolExecutionContext {
            workspace: Arc::clone(&self.workspace),
            events: self.events.clone(),
            call_id,
        };
        match tool.execute(arguments, context).await {
            Ok(result) => {
                let output = result.output.clone();
                let _ = self.events.send(ToolEvent::Finished {
                    call_id,
                    summary: result.summary,
                    display: result.display,
                });
                Ok(output)
            }
            Err(error) => {
                let message = error.to_string();
                let _ = self.events.send(ToolEvent::Failed {
                    call_id,
                    message: message.clone(),
                });
                if error.is_infrastructure() {
                    Err(error)
                } else {
                    Ok(format!("Error: {message}"))
                }
            }
        }
    }
}

#[derive(Clone)]
pub(crate) struct ToolRegistry {
    tools: Vec<Arc<dyn AgentTool>>,
}

impl fmt::Debug for ToolRegistry {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ToolRegistry")
            .field("names", &self.names())
            .finish()
    }
}

impl ToolRegistry {
    pub(crate) fn for_mode(mode: SessionMode) -> Self {
        Self::from_tools(
            mode,
            vec![
                Arc::new(read::ReadFileTool),
                Arc::new(search::SearchFilesTool),
                Arc::new(edit::EditFileTool),
                Arc::new(terminal::TerminalTool),
            ],
        )
        .expect("the built-in tool registry must have unique names")
    }

    fn from_tools(mode: SessionMode, tools: Vec<Arc<dyn AgentTool>>) -> Result<Self, ToolFailure> {
        let mut names = std::collections::HashSet::new();
        let mut available = Vec::new();
        for tool in tools {
            let name = tool.spec(mode).name;
            if !names.insert(name) {
                return Err(ToolFailure::infrastructure(format!(
                    "duplicate tool registration: {name}"
                )));
            }
            if tool.availability().includes(mode) {
                available.push(tool);
            }
        }
        Ok(Self { tools: available })
    }

    pub(crate) fn names(&self) -> Vec<&'static str> {
        self.tools
            .iter()
            .map(|tool| tool.spec(SessionMode::Build).name)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::{
        AgentTool, ToolDisplay, ToolEvent, ToolExecutionContext, ToolFailure, ToolRegistry,
        ToolResult, ToolSession, ToolSpec, read::ReadFileTool,
    };
    use crate::composer::SessionMode;
    use futures::future::BoxFuture;
    use serde_json::json;
    use std::{
        fs,
        path::Path,
        sync::Arc,
        time::{Duration, Instant},
    };
    use tokio::sync::mpsc;

    struct BrokenInfrastructureTool;

    impl AgentTool for BrokenInfrastructureTool {
        fn spec(&self, _mode: SessionMode) -> ToolSpec {
            ToolSpec {
                name: "broken_infrastructure",
                description: "test tool".into(),
                parameters: json!({ "type": "object" }),
            }
        }

        fn execute(
            &self,
            _arguments: String,
            _context: ToolExecutionContext,
        ) -> BoxFuture<'static, Result<ToolResult, ToolFailure>> {
            Box::pin(async { Err(ToolFailure::infrastructure("runner disappeared")) })
        }
    }

    #[test]
    fn registry_exposes_mutating_tools_only_in_build_mode() {
        let build = ToolRegistry::for_mode(SessionMode::Build);
        let plan = ToolRegistry::for_mode(SessionMode::Plan);

        assert_eq!(
            build.names(),
            ["read_file", "search_files", "edit_file", "terminal"]
        );
        assert_eq!(plan.names(), ["read_file", "search_files", "terminal"]);

        let duplicate = ToolRegistry::from_tools(
            SessionMode::Build,
            vec![Arc::new(ReadFileTool), Arc::new(ReadFileTool)],
        );
        assert!(duplicate.is_err());

        let edit = build
            .tools
            .iter()
            .find(|tool| tool.spec(SessionMode::Build).name == "edit_file")
            .expect("edit tool should be registered")
            .spec(SessionMode::Build);
        assert_eq!(edit.parameters["type"], "object");
        assert_eq!(
            edit.parameters["properties"]["operation"]["enum"],
            json!(["replace", "create"])
        );
    }

    #[tokio::test]
    async fn infrastructure_failures_escape_the_tool_session() {
        let root = tempfile::tempdir().expect("temporary workspace should be created");
        let (events_tx, mut events_rx) = mpsc::unbounded_channel();
        let session = ToolSession::new(root.path().to_owned(), SessionMode::Build, events_tx, 4)
            .expect("tool session should be created");

        let result = session
            .execute(Arc::new(BrokenInfrastructureTool), "{}".into())
            .await;

        assert!(result.is_err());
        assert!(matches!(
            events_rx.try_recv(),
            Ok(ToolEvent::Started { .. })
        ));
        assert!(matches!(events_rx.try_recv(), Ok(ToolEvent::Failed { .. })));
    }

    async fn execute(
        root: &Path,
        mode: SessionMode,
        name: &str,
        arguments: serde_json::Value,
    ) -> (String, Vec<ToolEvent>) {
        let (events_tx, mut events_rx) = mpsc::unbounded_channel();
        let session = ToolSession::new(root.to_owned(), mode, events_tx, 1)
            .expect("tool session should be created");
        let tool = session
            .tools()
            .into_iter()
            .find(|tool| session.spec(tool.as_ref()).name == name)
            .expect("requested tool should be registered");
        let output = session
            .execute(tool, arguments.to_string())
            .await
            .expect("tool infrastructure should remain available");
        let mut events = Vec::new();
        while let Ok(event) = events_rx.try_recv() {
            events.push(event);
        }
        (output, events)
    }

    #[tokio::test]
    async fn read_file_returns_numbered_ranges_and_a_code_display() {
        let root = tempfile::tempdir().expect("temporary workspace should be created");
        fs::write(root.path().join("notes.txt"), "one\ntwo\nthree\n")
            .expect("fixture should be written");

        let (output, events) = execute(
            root.path(),
            SessionMode::Build,
            "read_file",
            json!({ "path": "notes.txt", "start_line": 2, "end_line": 3 }),
        )
        .await;

        assert_eq!(output, "     2\ttwo\n     3\tthree");
        assert!(matches!(
            events.last(),
            Some(ToolEvent::Finished {
                display: ToolDisplay::CodeRange {
                    start_line: 2,
                    end_line: 3,
                    ..
                },
                ..
            })
        ));
    }

    #[tokio::test]
    async fn read_file_rejects_invalid_ranges_as_recoverable_failures() {
        let root = tempfile::tempdir().expect("temporary workspace should be created");
        fs::write(root.path().join("notes.txt"), "one\ntwo\n").expect("fixture should be written");

        let (output, events) = execute(
            root.path(),
            SessionMode::Build,
            "read_file",
            json!({ "path": "notes.txt", "start_line": 2, "end_line": 3 }),
        )
        .await;

        assert!(output.contains("invalid line range"));
        assert!(matches!(events.last(), Some(ToolEvent::Failed { .. })));
    }

    #[tokio::test]
    async fn search_files_honors_gitignore_and_returns_content_matches() {
        let root = tempfile::tempdir().expect("temporary workspace should be created");
        fs::create_dir(root.path().join("ignored")).expect("ignored directory should be created");
        fs::write(root.path().join(".gitignore"), "ignored/\n")
            .expect("ignore file should be written");
        fs::write(root.path().join("visible.rs"), "fn visible_marker() {}\n")
            .expect("visible fixture should be written");
        fs::write(
            root.path().join("ignored/hidden.rs"),
            "fn hidden_marker() {}\n",
        )
        .expect("hidden fixture should be written");

        let (output, _) = execute(
            root.path(),
            SessionMode::Build,
            "search_files",
            json!({ "mode": "content", "query": "marker", "glob": "**/*.rs" }),
        )
        .await;

        assert!(output.contains("visible.rs:1:fn visible_marker() {}"));
        assert!(!output.contains("hidden_marker"));
    }

    #[tokio::test]
    async fn search_limit_counts_matches_and_keeps_each_context_snippet_whole() {
        let root = tempfile::tempdir().expect("temporary workspace should be created");
        fs::write(
            root.path().join("values.txt"),
            "before one\nhit one\nafter one\nbefore two\nhit two\nafter two\nhit three\n",
        )
        .expect("fixture should be written");
        fs::write(root.path().join("binary.bin"), b"hit\0hidden")
            .expect("binary fixture should be written");

        let (output, events) = execute(
            root.path(),
            SessionMode::Build,
            "search_files",
            json!({
                "mode": "content",
                "query": "hit",
                "context_lines": 1,
                "limit": 2
            }),
        )
        .await;

        assert!(output.contains("values.txt-1-before one"));
        assert!(output.contains("values.txt-6-after two"));
        assert!(!output.contains("hit three"));
        assert!(!output.contains("binary.bin"));
        assert!(matches!(
            events.last(),
            Some(ToolEvent::Finished {
                summary: Some(summary),
                ..
            }) if summary == "Found 2 match(es)"
        ));
    }

    #[tokio::test]
    async fn edit_file_replaces_atomically_and_returns_a_unified_diff() {
        let root = tempfile::tempdir().expect("temporary workspace should be created");
        let path = root.path().join("value.txt");
        fs::write(&path, "before\n").expect("fixture should be written");

        let (output, events) = execute(
            root.path(),
            SessionMode::Build,
            "edit_file",
            json!({
                "operation": "replace",
                "path": "value.txt",
                "replacements": [{ "old_text": "before", "new_text": "after" }]
            }),
        )
        .await;

        assert_eq!(
            fs::read_to_string(path).expect("edited file should be read"),
            "after\n"
        );
        assert!(output.contains("-before"));
        assert!(output.contains("+after"));
        assert!(matches!(
            events.last(),
            Some(ToolEvent::Finished {
                display: ToolDisplay::Patch { .. },
                ..
            })
        ));
    }

    #[tokio::test]
    async fn edit_file_applies_multiple_disjoint_replacements_together() {
        let root = tempfile::tempdir().expect("temporary workspace should be created");
        let path = root.path().join("value.txt");
        fs::write(&path, "alpha middle omega\n").expect("fixture should be written");

        let (output, _) = execute(
            root.path(),
            SessionMode::Build,
            "edit_file",
            json!({
                "operation": "replace",
                "path": "value.txt",
                "replacements": [
                    { "old_text": "alpha", "new_text": "first" },
                    { "old_text": "omega", "new_text": "last" }
                ]
            }),
        )
        .await;

        assert_eq!(fs::read_to_string(path).unwrap(), "first middle last\n");
        assert!(output.contains("-alpha middle omega"));
        assert!(output.contains("+first middle last"));
    }

    #[tokio::test]
    async fn edit_file_rejects_ambiguous_text_without_writing() {
        let root = tempfile::tempdir().expect("temporary workspace should be created");
        let path = root.path().join("value.txt");
        fs::write(&path, "same\nsame\n").expect("fixture should be written");

        let (output, events) = execute(
            root.path(),
            SessionMode::Build,
            "edit_file",
            json!({
                "operation": "replace",
                "path": "value.txt",
                "replacements": [{ "old_text": "same", "new_text": "changed" }]
            }),
        )
        .await;

        assert!(output.starts_with("Error:"));
        assert_eq!(
            fs::read_to_string(path).expect("original should be read"),
            "same\nsame\n"
        );
        assert!(matches!(events.last(), Some(ToolEvent::Failed { .. })));
    }

    #[tokio::test]
    async fn edit_file_rejects_overlapping_replacements_without_writing() {
        let root = tempfile::tempdir().expect("temporary workspace should be created");
        let path = root.path().join("value.txt");
        fs::write(&path, "abcdef\n").expect("fixture should be written");

        let (output, _) = execute(
            root.path(),
            SessionMode::Build,
            "edit_file",
            json!({
                "operation": "replace",
                "path": "value.txt",
                "replacements": [
                    { "old_text": "abcdef", "new_text": "whole" },
                    { "old_text": "bcd", "new_text": "middle" }
                ]
            }),
        )
        .await;

        assert!(output.contains("replacement ranges overlap"));
        assert_eq!(fs::read_to_string(path).unwrap(), "abcdef\n");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn edit_file_preserves_existing_permissions() {
        use std::os::unix::fs::PermissionsExt as _;

        let root = tempfile::tempdir().expect("temporary workspace should be created");
        let path = root.path().join("script.sh");
        fs::write(&path, "echo before\n").expect("fixture should be written");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o751))
            .expect("fixture permissions should be set");

        execute(
            root.path(),
            SessionMode::Build,
            "edit_file",
            json!({
                "operation": "replace",
                "path": "script.sh",
                "replacements": [{ "old_text": "before", "new_text": "after" }]
            }),
        )
        .await;

        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o751
        );
    }

    #[tokio::test]
    async fn edit_file_creates_a_new_nested_file() {
        let root = tempfile::tempdir().expect("temporary workspace should be created");

        let (output, _) = execute(
            root.path(),
            SessionMode::Build,
            "edit_file",
            json!({
                "operation": "create",
                "path": "src/new.rs",
                "content": "pub fn new() {}\n"
            }),
        )
        .await;

        assert!(output.contains("+pub fn new() {}"));
        assert_eq!(
            fs::read_to_string(root.path().join("src/new.rs"))
                .expect("created file should be read"),
            "pub fn new() {}\n"
        );
    }

    #[tokio::test]
    async fn edit_file_create_fails_when_the_target_exists() {
        let root = tempfile::tempdir().expect("temporary workspace should be created");
        fs::write(root.path().join("existing.txt"), "original\n")
            .expect("fixture should be written");

        let (output, _) = execute(
            root.path(),
            SessionMode::Build,
            "edit_file",
            json!({
                "operation": "create",
                "path": "existing.txt",
                "content": "replacement\n"
            }),
        )
        .await;

        assert!(output.contains("already exists"));
        assert_eq!(
            fs::read_to_string(root.path().join("existing.txt")).unwrap(),
            "original\n"
        );
    }

    #[tokio::test]
    async fn terminal_streams_output_and_returns_nonzero_exit_status() {
        let root = tempfile::tempdir().expect("temporary workspace should be created");

        let (output, events) = execute(
            root.path(),
            SessionMode::Build,
            "terminal",
            json!({
                "description": "Check both streams",
                "command": "printf stdout; printf stderr >&2; exit 7"
            }),
        )
        .await;

        assert!(output.contains("stdout"));
        assert!(output.contains("stderr"));
        assert!(output.contains("[exit status: 7]"));
        assert!(
            events
                .iter()
                .any(|event| matches!(event, ToolEvent::OutputDelta { .. }))
        );
        assert!(matches!(
            events.last(),
            Some(ToolEvent::Finished {
                display: ToolDisplay::Terminal {
                    exit_code: Some(7),
                    ..
                },
                ..
            })
        ));
    }

    #[tokio::test]
    async fn terminal_marks_output_truncation_and_completes_empty_commands() {
        let root = tempfile::tempdir().expect("temporary workspace should be created");
        let (empty, _) = execute(
            root.path(),
            SessionMode::Build,
            "terminal",
            json!({ "description": "No output", "command": "true" }),
        )
        .await;
        assert_eq!(empty, "\n[exit status: 0]");

        let (large, _) = execute(
            root.path(),
            SessionMode::Build,
            "terminal",
            json!({
                "description": "Large output",
                "command": "head -c 1048600 /dev/zero | tr '\\0' x"
            }),
        )
        .await;
        assert_eq!(large.matches("[output truncated after 1 MiB]").count(), 1);
        assert!(large.ends_with("[exit status: 0]"));
    }

    #[tokio::test]
    async fn terminal_timeout_is_reported_as_a_recoverable_tool_failure() {
        let root = tempfile::tempdir().expect("temporary workspace should be created");
        let started = Instant::now();

        let (output, events) = execute(
            root.path(),
            SessionMode::Build,
            "terminal",
            json!({
                "description": "Wait too long",
                "command": "sleep 10 & echo $!; wait",
                "timeout_seconds": 1
            }),
        )
        .await;

        assert!(started.elapsed() < Duration::from_secs(4));
        assert!(output.contains("timed out after 1 second"));
        assert!(matches!(events.last(), Some(ToolEvent::Failed { .. })));
        #[cfg(unix)]
        {
            let child_pid = events
                .iter()
                .filter_map(|event| match event {
                    ToolEvent::OutputDelta { chunk, .. } => chunk.trim().parse::<i32>().ok(),
                    _ => None,
                })
                .next()
                .expect("the background child PID should be streamed");
            assert!(!process_exists(child_pid));
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cancelling_terminal_execution_kills_the_process_group() {
        let root = tempfile::tempdir().expect("temporary workspace should be created");
        let (events_tx, mut events_rx) = mpsc::unbounded_channel();
        let session = ToolSession::new(root.path().to_owned(), SessionMode::Build, events_tx, 3)
            .expect("tool session should be created");
        let tool = session
            .tools()
            .into_iter()
            .find(|tool| session.spec(tool.as_ref()).name == "terminal")
            .expect("terminal should be registered");
        let running_session = session.clone();
        let task = tokio::spawn(async move {
            running_session
                .execute(
                    tool,
                    json!({
                        "description": "Long process tree",
                        "command": "sleep 30 & echo $!; wait"
                    })
                    .to_string(),
                )
                .await
        });
        let child_pid = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if let Some(ToolEvent::OutputDelta { chunk, .. }) = events_rx.recv().await
                    && let Ok(pid) = chunk.trim().parse::<i32>()
                {
                    break pid;
                }
            }
        })
        .await
        .expect("the child PID should arrive");

        task.abort();
        let _ = task.await;
        tokio::time::sleep(Duration::from_millis(50)).await;

        assert!(!process_exists(child_pid));
    }

    #[cfg(unix)]
    fn process_exists(pid: i32) -> bool {
        // SAFETY: signal 0 only checks whether the PID is still present.
        unsafe { libc::kill(pid, 0) == 0 }
    }

    #[tokio::test]
    async fn plan_terminal_definition_is_explicitly_read_only() {
        let root = tempfile::tempdir().expect("temporary workspace should be created");
        let (events, _event_rx) = mpsc::unbounded_channel();
        let session = ToolSession::new(root.path().to_owned(), SessionMode::Plan, events, 2)
            .expect("tool session should be created");
        let terminal = session
            .tools()
            .into_iter()
            .find(|tool| session.spec(tool.as_ref()).name == "terminal")
            .expect("terminal should be registered");

        assert!(
            session
                .spec(terminal.as_ref())
                .description
                .contains("only non-mutating inspection commands")
        );
    }
}
