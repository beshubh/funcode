use super::{
    AgentTool, ToolDisplay, ToolExecutionContext, ToolFailure, ToolInvocation, ToolResult, ToolSpec,
};
use crate::session::SessionMode;
use futures::future::BoxFuture;
use serde::Deserialize;
use serde_json::json;
use std::{process::Stdio, time::Duration};
use tokio::{io::AsyncReadExt, process::Command, time::timeout};

const DEFAULT_TIMEOUT_SECONDS: u64 = 10 * 60;
const MAX_TIMEOUT_SECONDS: u64 = 30 * 60;
const MAX_OUTPUT_BYTES: usize = 1024 * 1024;
const TRUNCATION_MARKER: &str = "\n[output truncated after 1 MiB]\n";

#[derive(Debug, Deserialize)]
struct TerminalArgs {
    description: String,
    command: String,
    timeout_seconds: Option<u64>,
}

pub(super) struct TerminalTool;

impl AgentTool for TerminalTool {
    fn spec(&self, mode: SessionMode) -> ToolSpec {
        let mode_note = if mode == SessionMode::Plan {
            " Plan mode is active: use only non-mutating inspection commands."
        } else {
            ""
        };
        ToolSpec {
            name: "terminal",
            description: format!(
                "Run a non-interactive Bash command from the opened project and return combined stdout, stderr, and exit status.{mode_note}"
            ),
            parameters: json!({
                "type": "object",
                "properties": {
                    "description": { "type": "string", "description": "Short one-line description of what is being run" },
                    "command": { "type": "string", "minLength": 1 },
                    "timeout_seconds": { "type": "integer", "minimum": 1, "maximum": MAX_TIMEOUT_SECONDS }
                },
                "required": ["description", "command"],
                "additionalProperties": false
            }),
        }
    }

    fn invocation(&self, arguments: &str) -> ToolInvocation {
        serde_json::from_str::<TerminalArgs>(arguments)
            .map(|args| {
                let description = one_line(&args.description);
                ToolInvocation {
                    summary: description.clone(),
                    display: Some(ToolDisplay::Terminal {
                        description,
                        command: args.command,
                        output: String::new(),
                        exit_code: None,
                    }),
                }
            })
            .unwrap_or_else(|_| ToolInvocation {
                summary: "Running a terminal command".into(),
                display: None,
            })
    }

    fn execute(
        &self,
        arguments: String,
        context: ToolExecutionContext,
    ) -> BoxFuture<'static, Result<ToolResult, ToolFailure>> {
        Box::pin(async move {
            let args: TerminalArgs = serde_json::from_str(&arguments).map_err(|error| {
                ToolFailure::new(format!("invalid terminal arguments: {error}"))
            })?;
            if args.command.trim().is_empty() {
                return Err(ToolFailure::new("terminal command must not be empty"));
            }
            let description = one_line(&args.description);
            let timeout_seconds = args
                .timeout_seconds
                .unwrap_or(DEFAULT_TIMEOUT_SECONDS)
                .clamp(1, MAX_TIMEOUT_SECONDS);
            let (output, exit_code) = run_command(
                &args.command,
                Duration::from_secs(timeout_seconds),
                context.clone(),
            )
            .await?;
            let model_output = format!("{output}\n[exit status: {}]", exit_code_label(exit_code));
            Ok(ToolResult {
                output: model_output,
                display: ToolDisplay::Terminal {
                    description,
                    command: args.command,
                    output,
                    exit_code,
                },
                summary: Some(format!("Exited with {}", exit_code_label(exit_code))),
            })
        })
    }
}

async fn run_command(
    command: &str,
    duration: Duration,
    context: ToolExecutionContext,
) -> Result<(String, Option<i32>), ToolFailure> {
    let mut command_builder = Command::new("bash");
    command_builder
        .arg("-lc")
        .arg(command)
        .current_dir(context.workspace().root())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        command_builder.as_std_mut().process_group(0);
    }
    let mut child = command_builder
        .spawn()
        .map_err(|error| ToolFailure::infrastructure(format!("could not start Bash: {error}")))?;
    let mut process_group = ProcessGroupGuard::new(child.id());
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| ToolFailure::infrastructure("could not capture command stdout"))?;
    let mut stderr = child
        .stderr
        .take()
        .ok_or_else(|| ToolFailure::infrastructure("could not capture command stderr"))?;
    let mut output = String::new();
    let read_output = async {
        let mut stdout_open = true;
        let mut stderr_open = true;
        let mut truncated = false;
        let mut stdout_buffer = [0_u8; 4096];
        let mut stderr_buffer = [0_u8; 4096];
        while stdout_open || stderr_open {
            tokio::select! {
                read = stdout.read(&mut stdout_buffer), if stdout_open => {
                    let count = read.map_err(|error| ToolFailure::infrastructure(format!("could not read command stdout: {error}")))?;
                    stdout_open = count != 0;
                    if count != 0 {
                        append_output(&stdout_buffer[..count], &mut output, &mut truncated, &context);
                    }
                }
                read = stderr.read(&mut stderr_buffer), if stderr_open => {
                    let count = read.map_err(|error| ToolFailure::infrastructure(format!("could not read command stderr: {error}")))?;
                    stderr_open = count != 0;
                    if count != 0 {
                        append_output(&stderr_buffer[..count], &mut output, &mut truncated, &context);
                    }
                }
            }
        }
        child.wait().await.map_err(|error| {
            ToolFailure::infrastructure(format!("could not wait for command: {error}"))
        })
    };
    match timeout(duration, read_output).await {
        Ok(status) => {
            let status = status?;
            process_group.kill();
            process_group.disarm();
            Ok((output, status.code()))
        }
        Err(_) => {
            process_group.kill();
            let _ = child.wait().await;
            process_group.disarm();
            Err(ToolFailure::new(format!(
                "terminal command timed out after {} second(s)",
                duration.as_secs()
            )))
        }
    }
}

struct ProcessGroupGuard {
    pid: Option<u32>,
}

impl ProcessGroupGuard {
    fn new(pid: Option<u32>) -> Self {
        Self { pid }
    }

    fn kill(&self) {
        #[cfg(unix)]
        if let Some(pid) = self.pid {
            // SAFETY: a negative PID asks kill(2) to target the process group created above.
            unsafe {
                libc::kill(-(pid as i32), libc::SIGKILL);
            }
        }
    }

    fn disarm(&mut self) {
        self.pid = None;
    }
}

impl Drop for ProcessGroupGuard {
    fn drop(&mut self) {
        self.kill();
    }
}

fn append_output(
    bytes: &[u8],
    output: &mut String,
    truncated: &mut bool,
    context: &ToolExecutionContext,
) {
    if *truncated {
        return;
    }
    let content_limit = MAX_OUTPUT_BYTES - TRUNCATION_MARKER.len();
    let remaining = content_limit.saturating_sub(output.len());
    let decoded = String::from_utf8_lossy(bytes);
    let mut accepted = remaining.min(decoded.len());
    while !decoded.is_char_boundary(accepted) {
        accepted -= 1;
    }
    if accepted != 0 {
        let chunk = decoded[..accepted].to_owned();
        output.push_str(&chunk);
        context.output(chunk);
    }
    if accepted < decoded.len() || output.len() >= content_limit {
        output.push_str(TRUNCATION_MARKER);
        context.output(TRUNCATION_MARKER);
        *truncated = true;
    }
}

fn one_line(description: &str) -> String {
    description
        .lines()
        .next()
        .unwrap_or_default()
        .trim()
        .to_owned()
}

fn exit_code_label(exit_code: Option<i32>) -> String {
    exit_code
        .map(|code| code.to_string())
        .unwrap_or_else(|| "terminated by signal".into())
}
