use std::{
    collections::VecDeque,
    fmt,
    sync::mpsc::{self, Receiver, RecvTimeoutError, Sender},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

pub type RequestId = u64;

const FAKE_RESPONSE: &str =
    "This is a streamed placeholder response from funcode. Model integration will come next.";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentEvent {
    Started {
        request_id: RequestId,
    },
    TextDelta {
        request_id: RequestId,
        text: String,
    },
    ToolStarted {
        request_id: RequestId,
        name: String,
        summary: String,
    },
    ToolFinished {
        request_id: RequestId,
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
        prompt: String,
    },
    Cancel {
        request_id: RequestId,
    },
    Shutdown,
}

#[derive(Debug, Clone, Copy)]
struct RunnerTiming {
    thinking: Duration,
    chunk: Duration,
}

impl Default for RunnerTiming {
    fn default() -> Self {
        Self {
            thinking: Duration::from_millis(850),
            chunk: Duration::from_millis(110),
        }
    }
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
    commands: Sender<AgentCommand>,
    events: Receiver<AgentEvent>,
    thread: Option<JoinHandle<()>>,
}

impl AgentTaskRunner {
    pub fn spawn() -> Self {
        Self::spawn_with_timing(RunnerTiming::default())
    }

    fn spawn_with_timing(timing: RunnerTiming) -> Self {
        let (command_tx, command_rx) = mpsc::channel();
        let (event_tx, event_rx) = mpsc::channel();
        let thread = thread::spawn(move || run_coordinator(command_rx, event_tx, timing));

        Self {
            commands: command_tx,
            events: event_rx,
            thread: Some(thread),
        }
    }

    pub fn submit(&self, request_id: RequestId, prompt: String) -> Result<(), RunnerUnavailable> {
        self.commands
            .send(AgentCommand::Submit { request_id, prompt })
            .map_err(|_| RunnerUnavailable)
    }

    pub fn cancel(&self, request_id: RequestId) -> Result<(), RunnerUnavailable> {
        self.commands
            .send(AgentCommand::Cancel { request_id })
            .map_err(|_| RunnerUnavailable)
    }

    pub fn try_event(&self) -> Option<AgentEvent> {
        self.events.try_recv().ok()
    }

    #[cfg(test)]
    fn recv_timeout(&self, timeout: Duration) -> Result<AgentEvent, mpsc::RecvTimeoutError> {
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
    _prompt: String,
}

#[derive(Debug)]
enum Phase {
    Thinking,
    Streaming { chunks: Vec<String>, next: usize },
}

#[derive(Debug)]
struct ActiveRequest {
    request_id: RequestId,
    phase: Phase,
    deadline: Instant,
}

fn run_coordinator(
    commands: Receiver<AgentCommand>,
    events: Sender<AgentEvent>,
    timing: RunnerTiming,
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
            active = Some(ActiveRequest {
                request_id: request.request_id,
                phase: Phase::Thinking,
                deadline: Instant::now() + timing.thinking,
            });
        }

        let command = match active.as_ref() {
            Some(request) => {
                let timeout = request.deadline.saturating_duration_since(Instant::now());
                match commands.recv_timeout(timeout) {
                    Ok(command) => Some(command),
                    Err(RecvTimeoutError::Timeout) => {
                        advance_active(&mut active, &events, timing);
                        None
                    }
                    Err(RecvTimeoutError::Disconnected) => return,
                }
            }
            None => match commands.recv() {
                Ok(command) => Some(command),
                Err(_) => return,
            },
        };

        match command {
            Some(AgentCommand::Submit { request_id, prompt }) => {
                pending.push_back(PendingRequest {
                    request_id,
                    _prompt: prompt,
                });
            }
            Some(AgentCommand::Cancel { request_id }) => {
                if active.as_ref().map(|request| request.request_id) == Some(request_id) {
                    active = None;
                    if events.send(AgentEvent::Interrupted { request_id }).is_err() {
                        return;
                    }
                }
            }
            Some(AgentCommand::Shutdown) => return,
            None => {}
        }
    }
}

fn advance_active(
    active: &mut Option<ActiveRequest>,
    events: &Sender<AgentEvent>,
    timing: RunnerTiming,
) {
    let Some(request) = active.as_mut() else {
        return;
    };

    match &mut request.phase {
        Phase::Thinking => {
            request.phase = Phase::Streaming {
                chunks: response_chunks(),
                next: 0,
            };
            request.deadline = Instant::now();
        }
        Phase::Streaming { chunks, next } => {
            if let Some(text) = chunks.get(*next) {
                if events
                    .send(AgentEvent::TextDelta {
                        request_id: request.request_id,
                        text: text.clone(),
                    })
                    .is_err()
                {
                    *active = None;
                    return;
                }
                *next += 1;
                request.deadline = Instant::now() + timing.chunk;
            } else {
                let request_id = request.request_id;
                let _ = events.send(AgentEvent::Completed { request_id });
                *active = None;
            }
        }
    }
}

fn response_chunks() -> Vec<String> {
    let mut chunks = Vec::new();
    let mut words = FAKE_RESPONSE.split_whitespace().peekable();
    while let Some(word) = words.next() {
        let suffix = if words.peek().is_some() { " " } else { "" };
        chunks.push(format!("{word}{suffix}"));
    }
    chunks
}

#[cfg(test)]
mod tests {
    use super::{AgentEvent, AgentTaskRunner, RunnerTiming};
    use std::time::Duration;

    #[test]
    fn interrupting_the_active_request_continues_with_the_next_queued_request() {
        let mut runner = AgentTaskRunner::spawn_with_timing(RunnerTiming {
            thinking: Duration::from_secs(1),
            chunk: Duration::ZERO,
        });

        runner.submit(1, "first".into()).unwrap();
        runner.submit(2, "second".into()).unwrap();

        assert_eq!(
            runner.recv_timeout(Duration::from_millis(100)).unwrap(),
            AgentEvent::Started { request_id: 1 }
        );
        runner.cancel(1).unwrap();
        assert_eq!(
            runner.recv_timeout(Duration::from_millis(100)).unwrap(),
            AgentEvent::Interrupted { request_id: 1 }
        );
        assert_eq!(
            runner.recv_timeout(Duration::from_millis(100)).unwrap(),
            AgentEvent::Started { request_id: 2 }
        );

        runner.shutdown();
    }

    #[test]
    fn queued_requests_stream_to_completion_in_fifo_order() {
        let mut runner = AgentTaskRunner::spawn_with_timing(RunnerTiming {
            thinking: Duration::ZERO,
            chunk: Duration::ZERO,
        });
        runner.submit(1, "first".into()).unwrap();
        runner.submit(2, "second".into()).unwrap();

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
            } if text.contains("funcode")
        )));

        runner.shutdown();
    }
}
