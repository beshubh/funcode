use crate::{
    llm::{ConversationCommit, LlmClient, LlmEvent},
    submission::{PreparedAttachment, PreparedRequest},
    tools::{ToolDisplay, ToolEvent, ToolSession},
    transcript::{ToolArtifact, ToolCallId},
};
use futures::StreamExt as _;
use std::{
    collections::VecDeque,
    fmt,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
        mpsc::{self, Receiver, Sender},
    },
    thread::{self, JoinHandle},
};
use tokio::{sync::mpsc as async_mpsc, task::JoinHandle as AsyncJoinHandle};

pub type RequestId = u64;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentEvent {
    Started {
        request_id: RequestId,
    },
    TextDelta {
        request_id: RequestId,
        text: String,
    },
    ReasoningDelta {
        request_id: RequestId,
        summary: String,
    },
    ToolStarted {
        request_id: RequestId,
        call_id: ToolCallId,
        name: String,
        summary: String,
        artifacts: Vec<ToolArtifact>,
    },
    ToolOutputDelta {
        request_id: RequestId,
        call_id: ToolCallId,
        chunk: String,
    },
    ToolFinished {
        request_id: RequestId,
        call_id: ToolCallId,
        summary: Option<String>,
        artifacts: Vec<ToolArtifact>,
    },
    ToolFailed {
        request_id: RequestId,
        call_id: ToolCallId,
        message: String,
    },
    Completed {
        request_id: RequestId,
    },
    Interrupted {
        request_id: RequestId,
    },
    Failed {
        request_id: RequestId,
        message: String,
    },
}

#[derive(Debug)]
enum AgentCommand {
    Submit {
        request_id: RequestId,
        request: Arc<PreparedRequest>,
    },
    Cancel {
        request_id: RequestId,
    },
    Shutdown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RunnerUnavailable;

impl fmt::Display for RunnerUnavailable {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("the background agent runner is unavailable")
    }
}

impl std::error::Error for RunnerUnavailable {}

pub struct AgentTaskRunner {
    commands: async_mpsc::UnboundedSender<AgentCommand>,
    events: Receiver<AgentEvent>,
    outstanding: Arc<AtomicUsize>,
    thread: Option<JoinHandle<()>>,
}

impl AgentTaskRunner {
    #[cfg(test)]
    pub(crate) fn spawn(llm: LlmClient) -> Self {
        Self::spawn_in(
            llm,
            std::env::current_dir().expect("tests have a current directory"),
        )
    }

    pub(crate) fn spawn_in(llm: LlmClient, workspace_root: PathBuf) -> Self {
        let (command_tx, command_rx) = async_mpsc::unbounded_channel();
        let (event_tx, event_rx) = mpsc::channel();
        let outstanding = Arc::new(AtomicUsize::new(0));
        let coordinator_outstanding = Arc::clone(&outstanding);
        let thread = thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("failed to create the LLM runtime");
            runtime.block_on(run_coordinator(
                command_rx,
                event_tx,
                llm,
                workspace_root,
                coordinator_outstanding,
            ));
        });

        Self {
            commands: command_tx,
            events: event_rx,
            outstanding,
            thread: Some(thread),
        }
    }

    pub fn submit_prepared(
        &self,
        request_id: RequestId,
        request: Arc<PreparedRequest>,
    ) -> Result<(), RunnerUnavailable> {
        self.outstanding.fetch_add(1, Ordering::AcqRel);
        self.commands
            .send(AgentCommand::Submit {
                request_id,
                request,
            })
            .map_err(|_| {
                self.outstanding.fetch_sub(1, Ordering::AcqRel);
                RunnerUnavailable
            })
    }

    pub fn cancel(&self, request_id: RequestId) -> Result<(), RunnerUnavailable> {
        self.commands
            .send(AgentCommand::Cancel { request_id })
            .map_err(|_| RunnerUnavailable)
    }

    pub fn try_event(&self) -> Option<AgentEvent> {
        self.events.try_recv().ok()
    }

    pub fn is_idle(&self) -> bool {
        self.outstanding.load(Ordering::Acquire) == 0
    }

    #[cfg(test)]
    fn recv_timeout(
        &self,
        timeout: std::time::Duration,
    ) -> Result<AgentEvent, mpsc::RecvTimeoutError> {
        self.events.recv_timeout(timeout)
    }

    pub fn shutdown(&mut self) {
        let _ = self.commands.send(AgentCommand::Shutdown);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for AgentTaskRunner {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[derive(Debug)]
struct PendingRequest {
    request_id: RequestId,
    request: Arc<PreparedRequest>,
}

struct ActiveRequest {
    request_id: RequestId,
    task: AsyncJoinHandle<()>,
}

#[derive(Debug)]
enum RequestUpdate {
    TextDelta {
        request_id: RequestId,
        text: String,
    },
    ReasoningDelta {
        request_id: RequestId,
        summary: String,
    },
    ToolStarted {
        request_id: RequestId,
        call_id: ToolCallId,
        name: String,
        summary: String,
        artifacts: Vec<ToolArtifact>,
    },
    ToolOutputDelta {
        request_id: RequestId,
        call_id: ToolCallId,
        chunk: String,
    },
    ToolFinished {
        request_id: RequestId,
        call_id: ToolCallId,
        summary: Option<String>,
        artifacts: Vec<ToolArtifact>,
    },
    ToolFailed {
        request_id: RequestId,
        call_id: ToolCallId,
        message: String,
    },
    Completed {
        request_id: RequestId,
        commit: ConversationCommit,
    },
    Failed {
        request_id: RequestId,
        message: String,
    },
}

async fn run_coordinator(
    commands: async_mpsc::UnboundedReceiver<AgentCommand>,
    events: Sender<AgentEvent>,
    llm: LlmClient,
    workspace_root: PathBuf,
    outstanding: Arc<AtomicUsize>,
) {
    let (update_tx, update_rx) = async_mpsc::unbounded_channel();
    run_coordinator_with_updates(
        commands,
        events,
        llm,
        workspace_root,
        update_tx,
        update_rx,
        outstanding,
    )
    .await;
}

async fn run_coordinator_with_updates(
    mut commands: async_mpsc::UnboundedReceiver<AgentCommand>,
    events: Sender<AgentEvent>,
    llm: LlmClient,
    workspace_root: PathBuf,
    update_tx: async_mpsc::UnboundedSender<RequestUpdate>,
    mut update_rx: async_mpsc::UnboundedReceiver<RequestUpdate>,
    outstanding: Arc<AtomicUsize>,
) {
    let mut pending: VecDeque<PendingRequest> = VecDeque::new();
    let mut active: Option<ActiveRequest> = None;

    loop {
        if active.is_none()
            && let Some(request) = pending.pop_front()
        {
            if events
                .send(AgentEvent::Started {
                    request_id: request.request_id,
                })
                .is_err()
            {
                return;
            }
            let request_id = request.request_id;
            let task = tokio::spawn(run_request(
                request,
                llm.clone(),
                workspace_root.clone(),
                update_tx.clone(),
            ));
            active = Some(ActiveRequest { request_id, task });
        }

        tokio::select! {
            command = commands.recv() => match command {
                Some(AgentCommand::Submit { request_id, request }) => {
                    pending.push_back(PendingRequest { request_id, request });
                }
                Some(AgentCommand::Cancel { request_id }) => {
                    if active.as_ref().map(|request| request.request_id) == Some(request_id) {
                        if let Some(request) = active.take() {
                            request.task.abort();
                        }
                        if events.send(AgentEvent::Interrupted { request_id }).is_err() {
                            return;
                        }
                        outstanding.fetch_sub(1, Ordering::AcqRel);
                    }
                }
                Some(AgentCommand::Shutdown) | None => {
                    if let Some(request) = active.take() {
                        request.task.abort();
                    }
                    return;
                }
            },
            update = update_rx.recv() => {
                let Some(update) = update else {
                    return;
                };
                let request_id = match &update {
                    RequestUpdate::TextDelta { request_id, .. }
                    | RequestUpdate::ReasoningDelta { request_id, .. }
                    | RequestUpdate::ToolStarted { request_id, .. }
                    | RequestUpdate::ToolOutputDelta { request_id, .. }
                    | RequestUpdate::ToolFinished { request_id, .. }
                    | RequestUpdate::ToolFailed { request_id, .. }
                    | RequestUpdate::Completed { request_id, .. }
                    | RequestUpdate::Failed { request_id, .. } => *request_id,
                };
                if active.as_ref().map(|request| request.request_id) != Some(request_id) {
                    continue;
                }
                let event = match update {
                    RequestUpdate::TextDelta { request_id, text } => {
                        AgentEvent::TextDelta { request_id, text }
                    }
                    RequestUpdate::ReasoningDelta { request_id, summary } => {
                        AgentEvent::ReasoningDelta { request_id, summary }
                    }
                    RequestUpdate::ToolStarted { request_id, call_id, name, summary, artifacts } => {
                        AgentEvent::ToolStarted { request_id, call_id, name, summary, artifacts }
                    }
                    RequestUpdate::ToolOutputDelta { request_id, call_id, chunk } => {
                        AgentEvent::ToolOutputDelta { request_id, call_id, chunk }
                    }
                    RequestUpdate::ToolFinished { request_id, call_id, summary, artifacts } => {
                        AgentEvent::ToolFinished {
                            request_id,
                            call_id,
                            summary,
                            artifacts,
                        }
                    }
                    RequestUpdate::ToolFailed { request_id, call_id, message } => {
                        AgentEvent::ToolFailed { request_id, call_id, message }
                    }
                    RequestUpdate::Completed { request_id, commit } => {
                        active = None;
                        match llm.commit(commit) {
                            Ok(()) => AgentEvent::Completed { request_id },
                            Err(error) => AgentEvent::Failed {
                                request_id,
                                message: error.to_string(),
                            },
                        }
                    }
                    RequestUpdate::Failed { request_id, message } => {
                        active = None;
                        AgentEvent::Failed { request_id, message }
                    }
                };
                if matches!(
                    event,
                    AgentEvent::Completed { .. } | AgentEvent::Failed { .. }
                ) {
                    outstanding.fetch_sub(1, Ordering::AcqRel);
                }
                if events.send(event).is_err() {
                    return;
                }
            }
        }
    }
}

async fn run_request(
    request: PendingRequest,
    llm: LlmClient,
    workspace_root: PathBuf,
    updates: async_mpsc::UnboundedSender<RequestUpdate>,
) {
    let request_id = request.request_id;
    if emit_prepared_attachment_events(request_id, request.request.attachments(), &updates).is_err()
    {
        return;
    }
    let (tool_events_tx, mut tool_events_rx) = async_mpsc::unbounded_channel();
    let tools = match ToolSession::new(
        workspace_root,
        request.request.mode(),
        tool_events_tx,
        request_id,
    ) {
        Ok(tools) => tools,
        Err(error) => {
            let _ = updates.send(RequestUpdate::Failed {
                request_id,
                message: error.to_string(),
            });
            return;
        }
    };
    let mut stream = match llm
        .stream_prepared_with_tools(
            request.request.model_prompt().to_owned(),
            request.request.history_prompt().to_owned(),
            Some(tools),
        )
        .await
    {
        Ok(stream) => stream,
        Err(error) => {
            let _ = updates.send(RequestUpdate::Failed {
                request_id,
                message: error.to_string(),
            });
            return;
        }
    };

    loop {
        let event = tokio::select! {
            biased;
            Some(tool_event) = tool_events_rx.recv() => {
                if forward_tool_event(request_id, tool_event, &updates).is_err() {
                    return;
                }
                continue;
            }
            event = stream.next() => event,
        };
        let Some(event) = event else {
            break;
        };
        match event {
            Ok(LlmEvent::TextDelta(text)) => {
                if updates
                    .send(RequestUpdate::TextDelta { request_id, text })
                    .is_err()
                {
                    return;
                }
            }
            Ok(LlmEvent::ReasoningDelta(summary)) => {
                if updates
                    .send(RequestUpdate::ReasoningDelta {
                        request_id,
                        summary,
                    })
                    .is_err()
                {
                    return;
                }
            }
            Ok(LlmEvent::Completed(commit)) => {
                let _ = updates.send(RequestUpdate::Completed { request_id, commit });
                return;
            }
            Err(error) => {
                let _ = updates.send(RequestUpdate::Failed {
                    request_id,
                    message: error.to_string(),
                });
                return;
            }
        }
    }

    let _ = updates.send(RequestUpdate::Failed {
        request_id,
        message: "the model stream ended before completion".into(),
    });
}

fn forward_tool_event(
    request_id: RequestId,
    event: ToolEvent,
    updates: &async_mpsc::UnboundedSender<RequestUpdate>,
) -> Result<(), ()> {
    let update = match event {
        ToolEvent::Started {
            call_id,
            name,
            summary,
            display,
        } => RequestUpdate::ToolStarted {
            request_id,
            call_id,
            name,
            summary,
            artifacts: display.into_iter().map(display_to_artifact).collect(),
        },
        ToolEvent::OutputDelta { call_id, chunk } => RequestUpdate::ToolOutputDelta {
            request_id,
            call_id,
            chunk,
        },
        ToolEvent::Finished {
            call_id,
            summary,
            display,
        } => RequestUpdate::ToolFinished {
            request_id,
            call_id,
            summary,
            artifacts: vec![display_to_artifact(display)],
        },
        ToolEvent::Failed { call_id, message } => RequestUpdate::ToolFailed {
            request_id,
            call_id,
            message,
        },
    };
    updates.send(update).map_err(|_| ())
}

fn display_to_artifact(display: ToolDisplay) -> ToolArtifact {
    match display {
        ToolDisplay::CodeRange {
            path,
            start_line,
            end_line,
            content,
        } => ToolArtifact::CodeRange {
            path: path.into(),
            start_line,
            end_line,
            preview: Some(content),
        },
        ToolDisplay::SearchResults { query, matches } => {
            ToolArtifact::SearchResults { query, matches }
        }
        ToolDisplay::Patch { path, diff } => ToolArtifact::Patch {
            path: path.into(),
            diff,
        },
        ToolDisplay::Terminal {
            description,
            command,
            output,
            exit_code,
        } => ToolArtifact::Terminal {
            description,
            command,
            output,
            exit_code,
        },
    }
}

fn emit_prepared_attachment_events(
    request_id: RequestId,
    attachments: &[PreparedAttachment],
    updates: &async_mpsc::UnboundedSender<RequestUpdate>,
) -> Result<(), ()> {
    for (index, attachment) in attachments.iter().enumerate() {
        let call_id = attachment_call_id(request_id, index);
        let display_path = attachment.path().display();
        updates
            .send(RequestUpdate::ToolStarted {
                request_id,
                call_id,
                name: "read_workspace_file".into(),
                summary: format!("Reading {display_path}"),
                artifacts: Vec::new(),
            })
            .map_err(|_| ())?;
        updates
            .send(RequestUpdate::ToolFinished {
                request_id,
                call_id,
                summary: None,
                artifacts: vec![ToolArtifact::CodeRange {
                    path: attachment.path().clone(),
                    start_line: 1,
                    end_line: attachment.line_count(),
                    preview: Some(attachment.preview().to_owned()),
                }],
            })
            .map_err(|_| ())?;
    }
    Ok(())
}

fn attachment_call_id(request_id: RequestId, index: usize) -> ToolCallId {
    request_id.wrapping_shl(32).wrapping_add(index as u64)
}

#[cfg(test)]
mod tests {
    use super::{
        AgentCommand, AgentEvent, AgentTaskRunner, RequestUpdate, run_coordinator_with_updates,
    };
    use crate::composer::SubmittedContent;
    use crate::llm::{
        ConversationMessage, LlmClient, LlmError, Provider, ProviderEvent, ProviderRequest,
        ProviderStream,
    };
    use crate::session::SessionMode;
    use crate::submission::{PreparedRequest, SubmissionEvent, SubmissionTaskRunner};
    use crate::transcript::ToolArtifact;
    use crate::workspace::Attachment;
    use futures::{StreamExt as _, future::BoxFuture, stream};
    use std::{
        fs,
        sync::{Arc, atomic::AtomicUsize, mpsc},
        thread,
        time::Duration,
    };

    fn prepared(prompt: &str) -> Arc<PreparedRequest> {
        PreparedRequest::for_test(SubmittedContent::plain(prompt), SessionMode::Build)
    }

    fn submit(runner: &AgentTaskRunner, request_id: u64, prompt: &str) {
        runner
            .submit_prepared(request_id, prepared(prompt))
            .unwrap();
    }

    #[test]
    fn attached_workspace_files_emit_real_read_tool_events() {
        let workspace = tempfile::tempdir().unwrap();
        fs::write(workspace.path().join("project.txt"), "selected workspace").unwrap();
        let client = LlmClient::with_provider(Arc::new(EchoProvider));
        let mut runner = AgentTaskRunner::spawn_in(client, workspace.path().to_owned());
        let preflight = SubmissionTaskRunner::spawn(workspace.path().to_owned());
        preflight
            .request(
                3,
                SubmittedContent::with_attachments(
                    "Review this",
                    &[Attachment::workspace_file("project.txt")],
                ),
                SessionMode::Build,
            )
            .unwrap();
        let request = loop {
            if let Some(SubmissionEvent::Prepared { request, .. }) = preflight.try_event() {
                break request;
            }
            thread::sleep(Duration::from_millis(1));
        };
        fs::write(
            workspace.path().join("project.txt"),
            "changed after preflight",
        )
        .unwrap();
        runner.submit_prepared(3, request).unwrap();

        let mut events = Vec::new();
        while !events.contains(&AgentEvent::Completed { request_id: 3 }) {
            events.push(runner.recv_timeout(Duration::from_secs(1)).unwrap());
        }

        assert!(events.iter().any(|event| matches!(
            event,
            AgentEvent::ToolStarted { request_id: 3, name, .. }
                if name == "read_workspace_file"
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            AgentEvent::TextDelta { text, .. }
                if text.contains("selected workspace")
                    && !text.contains("changed after preflight")
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            AgentEvent::ToolFinished { request_id: 3, artifacts, .. }
                if matches!(
                    artifacts.as_slice(),
                    [ToolArtifact::CodeRange { path, .. }] if path == "project.txt"
                )
        )));

        runner.shutdown();
    }

    struct EchoProvider;

    impl Provider for EchoProvider {
        fn stream(
            &self,
            request: ProviderRequest,
        ) -> BoxFuture<'static, Result<ProviderStream, LlmError>> {
            Box::pin(async move {
                let response = format!("response to {}", request.prompt);
                let mut history = request.history;
                history.push(ConversationMessage::User(request.prompt));
                history.push(ConversationMessage::Assistant(response.clone()));
                Ok(Box::pin(stream::iter([
                    Ok(ProviderEvent::TextDelta(response)),
                    Ok(ProviderEvent::Completed(history)),
                ])) as ProviderStream)
            })
        }
    }

    struct BlockingFirstProvider;

    impl Provider for BlockingFirstProvider {
        fn stream(
            &self,
            request: ProviderRequest,
        ) -> BoxFuture<'static, Result<ProviderStream, LlmError>> {
            Box::pin(async move {
                if request.prompt == "first" {
                    Ok(Box::pin(stream::pending()) as ProviderStream)
                } else {
                    let mut history = request.history;
                    history.push(ConversationMessage::User(request.prompt));
                    history.push(ConversationMessage::Assistant("second response".into()));
                    Ok(Box::pin(stream::iter([
                        Ok(ProviderEvent::TextDelta("second response".into())),
                        Ok(ProviderEvent::Completed(history)),
                    ])) as ProviderStream)
                }
            })
        }
    }

    struct FailingFirstProvider;

    impl Provider for FailingFirstProvider {
        fn stream(
            &self,
            request: ProviderRequest,
        ) -> BoxFuture<'static, Result<ProviderStream, LlmError>> {
            Box::pin(async move {
                if request.prompt == "first" {
                    Ok(Box::pin(stream::iter([Err(LlmError::Provider(
                        "provider unavailable".into(),
                    ))])) as ProviderStream)
                } else {
                    let mut history = request.history;
                    history.push(ConversationMessage::User(request.prompt));
                    history.push(ConversationMessage::Assistant("recovered".into()));
                    Ok(Box::pin(stream::iter([
                        Ok(ProviderEvent::TextDelta("recovered".into())),
                        Ok(ProviderEvent::Completed(history)),
                    ])) as ProviderStream)
                }
            })
        }
    }

    struct MultiRoundToolProvider;

    impl Provider for MultiRoundToolProvider {
        fn stream(
            &self,
            request: ProviderRequest,
        ) -> BoxFuture<'static, Result<ProviderStream, LlmError>> {
            Box::pin(async move {
                let session = request
                    .tools
                    .expect("request-scoped tools should be present");
                let read_tool = session
                    .tools()
                    .into_iter()
                    .find(|tool| session.spec(tool.as_ref()).name == "read_file")
                    .expect("read_file should be registered");
                let failed_session = session.clone();
                let failed_tool = Arc::clone(&read_tool);
                let failed = stream::once(async move {
                    let output = failed_session
                        .execute(failed_tool, r#"{"path":"missing-for-tool-retry"}"#.into())
                        .await
                        .expect("domain failures should remain model-visible");
                    assert!(output.starts_with("Error:"));
                    Ok(ProviderEvent::TextDelta(String::new()))
                });
                let successful_session = session.clone();
                let successful = stream::once(async move {
                    let output = successful_session
                        .execute(
                            read_tool,
                            r#"{"path":"Cargo.toml","start_line":1,"end_line":1}"#.into(),
                        )
                        .await
                        .expect("retry should complete");
                    assert!(output.contains("[package]"));
                    Ok(ProviderEvent::TextDelta(String::new()))
                });
                let mut history = request.history;
                history.push(ConversationMessage::User(request.prompt));
                history.push(ConversationMessage::Assistant("before after".into()));
                let events = stream::iter([Ok(ProviderEvent::TextDelta("before ".into()))])
                    .chain(failed)
                    .chain(successful)
                    .chain(stream::iter([
                        Ok(ProviderEvent::TextDelta("after".into())),
                        Ok(ProviderEvent::Completed(history)),
                    ]));
                Ok(Box::pin(events) as ProviderStream)
            })
        }
    }

    #[test]
    fn queued_requests_stream_to_completion_in_fifo_order() {
        let client = LlmClient::with_provider(Arc::new(EchoProvider));
        let mut runner = AgentTaskRunner::spawn(client);
        submit(&runner, 1, "first");
        submit(&runner, 2, "second");
        assert!(!runner.is_idle());

        let mut events = Vec::new();
        while !events.contains(&AgentEvent::Completed { request_id: 2 }) {
            events.push(runner.recv_timeout(Duration::from_secs(1)).unwrap());
        }

        let first_completed = events
            .iter()
            .position(|event| *event == AgentEvent::Completed { request_id: 1 })
            .unwrap();
        let second_started = events
            .iter()
            .position(|event| *event == AgentEvent::Started { request_id: 2 })
            .unwrap();
        assert!(first_completed < second_started);
        assert!(events.iter().any(|event| matches!(
            event,
            AgentEvent::TextDelta {
                request_id: 1,
                text
            } if text == "response to first"
        )));
        assert!(runner.is_idle());

        runner.shutdown();
    }

    #[test]
    fn tool_failure_can_retry_across_rounds_with_text_before_and_after() {
        let client = LlmClient::with_provider(Arc::new(MultiRoundToolProvider));
        let mut runner = AgentTaskRunner::spawn(client);
        submit(&runner, 21, "inspect");

        let mut events = Vec::new();
        while !events.contains(&AgentEvent::Completed { request_id: 21 }) {
            events.push(runner.recv_timeout(Duration::from_secs(2)).unwrap());
        }

        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, AgentEvent::ToolStarted { .. }))
                .count(),
            2
        );
        assert!(
            events
                .iter()
                .any(|event| matches!(event, AgentEvent::ToolFailed { request_id: 21, .. }))
        );
        assert!(
            events
                .iter()
                .any(|event| matches!(event, AgentEvent::ToolFinished { request_id: 21, .. }))
        );
        let text = events
            .iter()
            .filter_map(|event| match event {
                AgentEvent::TextDelta { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect::<String>();
        assert_eq!(text, "before after");

        runner.shutdown();
    }

    #[test]
    fn interrupting_the_active_request_continues_with_the_next_queued_request() {
        let client = LlmClient::with_provider(Arc::new(BlockingFirstProvider));
        let mut runner = AgentTaskRunner::spawn(client);
        submit(&runner, 1, "first");
        submit(&runner, 2, "second");

        assert_eq!(
            runner.recv_timeout(Duration::from_secs(1)).unwrap(),
            AgentEvent::Started { request_id: 1 }
        );
        runner.cancel(1).unwrap();
        assert_eq!(
            runner.recv_timeout(Duration::from_secs(1)).unwrap(),
            AgentEvent::Interrupted { request_id: 1 }
        );
        assert_eq!(
            runner.recv_timeout(Duration::from_secs(1)).unwrap(),
            AgentEvent::Started { request_id: 2 }
        );

        let mut events = Vec::new();
        while !events.contains(&AgentEvent::Completed { request_id: 2 }) {
            events.push(runner.recv_timeout(Duration::from_secs(1)).unwrap());
        }
        assert!(events.iter().any(|event| matches!(
            event,
            AgentEvent::TextDelta { request_id: 2, text } if text == "second response"
        )));

        runner.shutdown();
    }

    #[test]
    fn provider_failure_only_fails_the_active_request() {
        let client = LlmClient::with_provider(Arc::new(FailingFirstProvider));
        let mut runner = AgentTaskRunner::spawn(client);
        submit(&runner, 1, "first");
        submit(&runner, 2, "second");

        let mut events = Vec::new();
        while !events.contains(&AgentEvent::Completed { request_id: 2 }) {
            events.push(runner.recv_timeout(Duration::from_secs(1)).unwrap());
        }

        assert!(events.iter().any(|event| matches!(
            event,
            AgentEvent::Failed { request_id: 1, message } if message == "provider unavailable"
        )));
        assert!(events.contains(&AgentEvent::Started { request_id: 2 }));
        assert!(events.iter().any(|event| matches!(
            event,
            AgentEvent::TextDelta { request_id: 2, text } if text == "recovered"
        )));

        runner.shutdown();
    }

    #[test]
    fn late_updates_after_cancellation_are_discarded() {
        let client = LlmClient::with_provider(Arc::new(BlockingFirstProvider));
        let (command_tx, command_rx) = tokio::sync::mpsc::unbounded_channel();
        let (event_tx, event_rx) = mpsc::channel();
        let (update_tx, update_rx) = tokio::sync::mpsc::unbounded_channel();
        let coordinator_update_tx = update_tx.clone();
        let workspace_root = std::env::current_dir().unwrap();
        let coordinator = thread::spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(run_coordinator_with_updates(
                    command_rx,
                    event_tx,
                    client,
                    workspace_root,
                    coordinator_update_tx,
                    update_rx,
                    Arc::new(AtomicUsize::new(2)),
                ));
        });

        command_tx
            .send(AgentCommand::Submit {
                request_id: 1,
                request: prepared("first"),
            })
            .unwrap();
        assert_eq!(
            event_rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            AgentEvent::Started { request_id: 1 }
        );
        command_tx
            .send(AgentCommand::Cancel { request_id: 1 })
            .unwrap();
        assert_eq!(
            event_rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            AgentEvent::Interrupted { request_id: 1 }
        );

        update_tx
            .send(RequestUpdate::TextDelta {
                request_id: 1,
                text: "stale".into(),
            })
            .unwrap();
        command_tx
            .send(AgentCommand::Submit {
                request_id: 2,
                request: prepared("second"),
            })
            .unwrap();

        let mut events = Vec::new();
        while !events.contains(&AgentEvent::Completed { request_id: 2 }) {
            events.push(event_rx.recv_timeout(Duration::from_secs(1)).unwrap());
        }
        assert!(!events.iter().any(|event| matches!(
            event,
            AgentEvent::TextDelta { request_id: 1, text } if text == "stale"
        )));

        command_tx.send(AgentCommand::Shutdown).unwrap();
        coordinator.join().unwrap();
    }
}
