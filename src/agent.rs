use crate::llm::{ConversationCommit, LlmClient, LlmEvent};
use futures::StreamExt as _;
use std::{
    collections::VecDeque,
    fmt,
    sync::mpsc::{self, Receiver, Sender},
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
    thread: Option<JoinHandle<()>>,
}

impl AgentTaskRunner {
    pub(crate) fn spawn(llm: LlmClient) -> Self {
        let (command_tx, command_rx) = async_mpsc::unbounded_channel();
        let (event_tx, event_rx) = mpsc::channel();
        let thread = thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("failed to create the LLM runtime");
            runtime.block_on(run_coordinator(command_rx, event_tx, llm));
        });

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
    prompt: String,
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
) {
    let (update_tx, update_rx) = async_mpsc::unbounded_channel();
    run_coordinator_with_updates(commands, events, llm, update_tx, update_rx).await;
}

async fn run_coordinator_with_updates(
    mut commands: async_mpsc::UnboundedReceiver<AgentCommand>,
    events: Sender<AgentEvent>,
    llm: LlmClient,
    update_tx: async_mpsc::UnboundedSender<RequestUpdate>,
    mut update_rx: async_mpsc::UnboundedReceiver<RequestUpdate>,
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
            let task = tokio::spawn(run_request(request, llm.clone(), update_tx.clone()));
            active = Some(ActiveRequest { request_id, task });
        }

        tokio::select! {
            command = commands.recv() => match command {
                Some(AgentCommand::Submit { request_id, prompt }) => {
                    pending.push_back(PendingRequest { request_id, prompt });
                }
                Some(AgentCommand::Cancel { request_id }) => {
                    if active.as_ref().map(|request| request.request_id) == Some(request_id) {
                        if let Some(request) = active.take() {
                            request.task.abort();
                        }
                        if events.send(AgentEvent::Interrupted { request_id }).is_err() {
                            return;
                        }
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
    updates: async_mpsc::UnboundedSender<RequestUpdate>,
) {
    let request_id = request.request_id;
    let mut stream = match llm.stream(request.prompt).await {
        Ok(stream) => stream,
        Err(error) => {
            let _ = updates.send(RequestUpdate::Failed {
                request_id,
                message: error.to_string(),
            });
            return;
        }
    };

    while let Some(event) = stream.next().await {
        match event {
            Ok(LlmEvent::TextDelta(text)) => {
                if updates
                    .send(RequestUpdate::TextDelta { request_id, text })
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

#[cfg(test)]
mod tests {
    use super::{
        AgentCommand, AgentEvent, AgentTaskRunner, RequestUpdate, run_coordinator_with_updates,
    };
    use crate::llm::{
        ConversationMessage, LlmClient, LlmError, Provider, ProviderEvent, ProviderRequest,
        ProviderStream,
    };
    use futures::{future::BoxFuture, stream};
    use std::{
        sync::{Arc, mpsc},
        thread,
        time::Duration,
    };

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

    #[test]
    fn queued_requests_stream_to_completion_in_fifo_order() {
        let client = LlmClient::with_provider(Arc::new(EchoProvider));
        let mut runner = AgentTaskRunner::spawn(client);
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
            } if text == "response to first"
        )));

        runner.shutdown();
    }

    #[test]
    fn interrupting_the_active_request_continues_with_the_next_queued_request() {
        let client = LlmClient::with_provider(Arc::new(BlockingFirstProvider));
        let mut runner = AgentTaskRunner::spawn(client);
        runner.submit(1, "first".into()).unwrap();
        runner.submit(2, "second".into()).unwrap();

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
        runner.submit(1, "first".into()).unwrap();
        runner.submit(2, "second".into()).unwrap();

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
        let coordinator = thread::spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(run_coordinator_with_updates(
                    command_rx,
                    event_tx,
                    client,
                    coordinator_update_tx,
                    update_rx,
                ));
        });

        command_tx
            .send(AgentCommand::Submit {
                request_id: 1,
                prompt: "first".into(),
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
                prompt: "second".into(),
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
