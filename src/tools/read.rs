use super::{
    AgentTool, ToolDisplay, ToolExecutionContext, ToolFailure, ToolInvocation, ToolResult, ToolSpec,
};
use crate::{session::SessionMode, workspace::WorkspacePath};
use futures::future::BoxFuture;
use serde::Deserialize;
use serde_json::json;
use std::fs;

const MAX_READ_BYTES: u64 = 1024 * 1024;

#[derive(Debug, Deserialize)]
struct ReadFileArgs {
    path: String,
    start_line: Option<u32>,
    end_line: Option<u32>,
}

pub(super) struct ReadFileTool;

impl AgentTool for ReadFileTool {
    fn spec(&self, _mode: SessionMode) -> ToolSpec {
        ToolSpec {
            name: "read_file",
            description: "Read a UTF-8 file inside the opened workspace, optionally selecting an inclusive line range. Requested bounds beyond the file are clamped to its available lines.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Workspace-relative file path" },
                    "start_line": { "type": "integer", "minimum": 1 },
                    "end_line": { "type": "integer", "minimum": 1 }
                },
                "required": ["path"],
                "additionalProperties": false
            }),
        }
    }

    fn invocation(&self, arguments: &str) -> ToolInvocation {
        serde_json::from_str::<ReadFileArgs>(arguments)
            .map(|args| ToolInvocation {
                summary: format!("Reading {}", WorkspacePath::from_raw(args.path).display()),
                display: None,
            })
            .unwrap_or_else(|_| ToolInvocation {
                summary: "Reading a workspace file".into(),
                display: None,
            })
    }

    fn execute(
        &self,
        arguments: String,
        context: ToolExecutionContext,
    ) -> BoxFuture<'static, Result<ToolResult, ToolFailure>> {
        Box::pin(async move {
            tokio::task::spawn_blocking(move || read_file(arguments, context))
                .await
                .map_err(|error| {
                    ToolFailure::infrastructure(format!("read worker failed: {error}"))
                })?
        })
    }
}

fn read_file(arguments: String, context: ToolExecutionContext) -> Result<ToolResult, ToolFailure> {
    let args: ReadFileArgs = serde_json::from_str(&arguments)
        .map_err(|error| ToolFailure::new(format!("invalid read_file arguments: {error}")))?;
    let display_path = WorkspacePath::from_raw(args.path.clone()).display();
    let path = context.workspace().existing_file(&args.path)?;
    let metadata = fs::metadata(&path).map_err(|error| {
        ToolFailure::new(format!("could not inspect '{display_path}': {error}"))
    })?;
    if metadata.len() > MAX_READ_BYTES {
        return Err(ToolFailure::new(format!(
            "'{}' exceeds the {} KiB read limit",
            display_path,
            MAX_READ_BYTES / 1024
        )));
    }
    let content = fs::read_to_string(&path).map_err(|error| {
        ToolFailure::new(format!(
            "could not read '{}' as UTF-8 text: {error}",
            display_path
        ))
    })?;
    let lines = content.lines().collect::<Vec<_>>();
    let total = lines.len().max(1) as u32;
    let start = args.start_line.unwrap_or(1).clamp(1, total);
    let end = args.end_line.unwrap_or(total).clamp(start, total);
    let selected = if lines.is_empty() {
        String::new()
    } else {
        lines[(start - 1) as usize..end as usize]
            .iter()
            .enumerate()
            .map(|(offset, line)| format!("{:>6}\t{}", start as usize + offset, line))
            .collect::<Vec<_>>()
            .join("\n")
    };
    Ok(ToolResult {
        output: selected.clone(),
        display: ToolDisplay::CodeRange {
            path: args.path,
            start_line: start,
            end_line: end,
            content: selected,
        },
        summary: None,
    })
}
