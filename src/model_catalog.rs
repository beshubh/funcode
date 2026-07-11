use crate::llm::{LlmClient, ProviderModels};
use std::{
    fmt,
    sync::mpsc::{self, Receiver, Sender},
    thread::{self, JoinHandle},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ModelCatalogEvent {
    Loaded(Vec<ProviderModels>),
    Failed(String),
}

enum ModelCatalogCommand {
    Load,
    Shutdown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ModelCatalogUnavailable;

impl fmt::Display for ModelCatalogUnavailable {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("the background model catalog runner is unavailable")
    }
}

impl std::error::Error for ModelCatalogUnavailable {}

pub(crate) struct ModelCatalogTaskRunner {
    commands: Sender<ModelCatalogCommand>,
    events: Receiver<ModelCatalogEvent>,
    thread: Option<JoinHandle<()>>,
}

impl ModelCatalogTaskRunner {
    pub(crate) fn spawn(client: LlmClient) -> Self {
        let (command_tx, command_rx) = mpsc::channel();
        let (event_tx, event_rx) = mpsc::channel();
        let thread = thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("failed to create the model catalog runtime");
            while let Ok(command) = command_rx.recv() {
                match command {
                    ModelCatalogCommand::Load => {
                        let event = match runtime.block_on(client.list_models()) {
                            Ok(catalogs) => ModelCatalogEvent::Loaded(catalogs),
                            Err(error) => ModelCatalogEvent::Failed(error.to_string()),
                        };
                        if event_tx.send(event).is_err() {
                            return;
                        }
                    }
                    ModelCatalogCommand::Shutdown => return,
                }
            }
        });
        Self {
            commands: command_tx,
            events: event_rx,
            thread: Some(thread),
        }
    }

    pub(crate) fn load(&self) -> Result<(), ModelCatalogUnavailable> {
        self.commands
            .send(ModelCatalogCommand::Load)
            .map_err(|_| ModelCatalogUnavailable)
    }

    pub(crate) fn try_event(&self) -> Option<ModelCatalogEvent> {
        self.events.try_recv().ok()
    }

    pub(crate) fn shutdown(&mut self) {
        let _ = self.commands.send(ModelCatalogCommand::Shutdown);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }

    #[cfg(test)]
    fn recv_timeout(
        &self,
        timeout: std::time::Duration,
    ) -> Result<ModelCatalogEvent, mpsc::RecvTimeoutError> {
        self.events.recv_timeout(timeout)
    }
}

impl Drop for ModelCatalogTaskRunner {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::{ModelCatalogEvent, ModelCatalogTaskRunner};
    use crate::llm::{
        LlmClient, LlmError, ModelInfo, Provider, ProviderModels, ProviderRequest, ProviderStream,
    };
    use futures::future::BoxFuture;
    use std::{sync::Arc, time::Duration};

    struct CatalogProvider;

    impl Provider for CatalogProvider {
        fn stream(
            &self,
            _request: ProviderRequest,
        ) -> BoxFuture<'static, Result<ProviderStream, LlmError>> {
            Box::pin(async { Err(LlmError::Internal("not used".into())) })
        }

        fn list_models(&self) -> BoxFuture<'static, Result<ProviderModels, LlmError>> {
            Box::pin(async {
                Ok(ProviderModels {
                    provider: "Test".into(),
                    source: "built-in catalog".into(),
                    models: vec![ModelInfo {
                        id: "test-model".into(),
                        display_name: "Test Model".into(),
                    }],
                })
            })
        }
    }

    struct FailingCatalogProvider;

    impl Provider for FailingCatalogProvider {
        fn stream(
            &self,
            _request: ProviderRequest,
        ) -> BoxFuture<'static, Result<ProviderStream, LlmError>> {
            Box::pin(async { Err(LlmError::Internal("not used".into())) })
        }

        fn list_models(&self) -> BoxFuture<'static, Result<ProviderModels, LlmError>> {
            Box::pin(async { Err(LlmError::Provider("catalog unavailable".into())) })
        }
    }

    #[test]
    fn model_catalog_is_loaded_off_the_terminal_thread() {
        let client = LlmClient::with_provider(Arc::new(CatalogProvider));
        let mut runner = ModelCatalogTaskRunner::spawn(client);

        runner.load().unwrap();

        assert!(matches!(
            runner.recv_timeout(Duration::from_secs(1)).unwrap(),
            ModelCatalogEvent::Loaded(catalogs)
                if catalogs[0].models[0].id == "test-model"
        ));
        runner.shutdown();
    }

    #[test]
    fn provider_failure_is_returned_to_the_terminal_thread() {
        let client = LlmClient::with_provider(Arc::new(FailingCatalogProvider));
        let mut runner = ModelCatalogTaskRunner::spawn(client);

        runner.load().unwrap();

        assert_eq!(
            runner.recv_timeout(Duration::from_secs(1)).unwrap(),
            ModelCatalogEvent::Failed("catalog unavailable".into())
        );
        runner.shutdown();
    }
}
