use crate::llm::{LlmClient, ProviderModels};
use std::{
    fmt, fs,
    path::{Path, PathBuf},
    sync::mpsc::{self, Receiver, Sender},
    thread::{self, JoinHandle},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

const CACHE_VERSION: u8 = 1;
const CACHE_FRESHNESS: Duration = Duration::from_secs(24 * 60 * 60);

#[derive(serde::Deserialize, serde::Serialize)]
struct CachedCatalogs {
    version: u8,
    fetched_at: u64,
    catalogs: Vec<ProviderModels>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ModelCatalogEvent {
    Loaded(Vec<ProviderModels>),
    Failed(String),
}

enum ModelCatalogCommand {
    Load { refresh: bool },
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
    client: LlmClient,
    commands: Sender<ModelCatalogCommand>,
    events: Receiver<ModelCatalogEvent>,
    thread: Option<JoinHandle<()>>,
}

impl ModelCatalogTaskRunner {
    pub(crate) fn spawn(client: LlmClient) -> Self {
        Self::spawn_with_cache(client, standard_cache_path(), CACHE_FRESHNESS)
    }

    fn spawn_with_cache(client: LlmClient, cache_path: PathBuf, freshness: Duration) -> Self {
        let (command_tx, command_rx) = mpsc::channel();
        let (event_tx, event_rx) = mpsc::channel();
        let catalog_client = client.clone();
        let thread = thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("failed to create the model catalog runtime");
            while let Ok(command) = command_rx.recv() {
                match command {
                    ModelCatalogCommand::Load { refresh } => {
                        let event = match load_catalogs(
                            &runtime,
                            &catalog_client,
                            &cache_path,
                            freshness,
                            refresh,
                        ) {
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
            client,
            commands: command_tx,
            events: event_rx,
            thread: Some(thread),
        }
    }

    pub(crate) fn load(&self) -> Result<(), ModelCatalogUnavailable> {
        self.commands
            .send(ModelCatalogCommand::Load { refresh: false })
            .map_err(|_| ModelCatalogUnavailable)
    }

    pub(crate) fn refresh(&self) -> Result<(), ModelCatalogUnavailable> {
        self.commands
            .send(ModelCatalogCommand::Load { refresh: true })
            .map_err(|_| ModelCatalogUnavailable)
    }

    pub(crate) fn select_model(&self, model: String) -> Result<(), crate::llm::LlmError> {
        self.client.select_model(model)
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

fn standard_cache_path() -> PathBuf {
    std::env::var_os("FUNCODE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".funcode")))
        .or_else(|| {
            std::env::var_os("USERPROFILE").map(|home| PathBuf::from(home).join(".funcode"))
        })
        .unwrap_or_else(|| std::env::temp_dir().join(".funcode"))
        .join("models.json")
}

fn load_catalogs(
    runtime: &tokio::runtime::Runtime,
    client: &LlmClient,
    cache_path: &Path,
    freshness: Duration,
    refresh: bool,
) -> Result<Vec<ProviderModels>, crate::llm::LlmError> {
    if !refresh && let Some(catalogs) = read_fresh_cache(cache_path, freshness) {
        return Ok(catalogs);
    }

    let catalogs = runtime.block_on(client.list_models())?;
    write_cache(cache_path, &catalogs).map_err(|error| {
        crate::llm::LlmError::Internal(format!("could not save the model catalog: {error}"))
    })?;
    Ok(catalogs)
}

fn read_fresh_cache(path: &Path, freshness: Duration) -> Option<Vec<ProviderModels>> {
    let cache: CachedCatalogs = serde_json::from_slice(&fs::read(path).ok()?).ok()?;
    if cache.version != CACHE_VERSION {
        return None;
    }
    let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs();
    if now.saturating_sub(cache.fetched_at) > freshness.as_secs() {
        return None;
    }
    Some(
        cache
            .catalogs
            .into_iter()
            .map(|mut catalog| {
                catalog.source = "cached catalog".into();
                catalog
            })
            .collect(),
    )
}

fn write_cache(path: &Path, catalogs: &[ProviderModels]) -> std::io::Result<()> {
    let parent = path.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "model cache path has no parent directory",
        )
    })?;
    fs::create_dir_all(parent)?;
    let fetched_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let bytes = serde_json::to_vec_pretty(&CachedCatalogs {
        version: CACHE_VERSION,
        fetched_at,
        catalogs: catalogs.to_vec(),
    })
    .map_err(std::io::Error::other)?;
    fs::write(path, bytes)
}

impl Drop for ModelCatalogTaskRunner {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::{CACHE_VERSION, CachedCatalogs, ModelCatalogEvent, ModelCatalogTaskRunner};
    use crate::llm::{
        LlmClient, LlmError, ModelInfo, Provider, ProviderModels, ProviderRequest, ProviderStream,
    };
    use futures::future::BoxFuture;
    use std::{
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        time::Duration,
    };

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

    struct CountingCatalogProvider {
        calls: Arc<AtomicUsize>,
    }

    impl Provider for CountingCatalogProvider {
        fn stream(
            &self,
            _request: ProviderRequest,
        ) -> BoxFuture<'static, Result<ProviderStream, LlmError>> {
            Box::pin(async { Err(LlmError::Internal("not used".into())) })
        }

        fn list_models(&self) -> BoxFuture<'static, Result<ProviderModels, LlmError>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async {
                Ok(ProviderModels {
                    provider: "Test".into(),
                    source: "live provider API".into(),
                    models: vec![ModelInfo {
                        id: "test-model".into(),
                        display_name: "Test Model".into(),
                    }],
                })
            })
        }
    }

    fn cache_path() -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "funcode-model-cache-{}-{}.json",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    #[test]
    fn model_catalog_is_loaded_off_the_terminal_thread() {
        let client = LlmClient::with_provider(Arc::new(CatalogProvider));
        let path = cache_path();
        let mut runner = ModelCatalogTaskRunner::spawn_with_cache(
            client,
            path.clone(),
            Duration::from_secs(24 * 60 * 60),
        );

        runner.load().unwrap();

        assert!(matches!(
            runner.recv_timeout(Duration::from_secs(1)).unwrap(),
            ModelCatalogEvent::Loaded(catalogs)
                if catalogs[0].models[0].id == "test-model"
        ));
        runner.shutdown();
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn provider_failure_is_returned_to_the_terminal_thread() {
        let client = LlmClient::with_provider(Arc::new(FailingCatalogProvider));
        let path = cache_path();
        let mut runner = ModelCatalogTaskRunner::spawn_with_cache(
            client,
            path.clone(),
            Duration::from_secs(24 * 60 * 60),
        );

        runner.load().unwrap();

        assert_eq!(
            runner.recv_timeout(Duration::from_secs(1)).unwrap(),
            ModelCatalogEvent::Failed("catalog unavailable".into())
        );
        runner.shutdown();
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn fresh_cache_avoids_a_second_provider_request() {
        let calls = Arc::new(AtomicUsize::new(0));
        let client = LlmClient::with_provider(Arc::new(CountingCatalogProvider {
            calls: Arc::clone(&calls),
        }));
        let path = cache_path();
        let mut runner = ModelCatalogTaskRunner::spawn_with_cache(
            client,
            path.clone(),
            Duration::from_secs(24 * 60 * 60),
        );

        runner.load().unwrap();
        let first = runner.recv_timeout(Duration::from_secs(1)).unwrap();
        runner.load().unwrap();
        let second = runner.recv_timeout(Duration::from_secs(1)).unwrap();

        assert!(matches!(first, ModelCatalogEvent::Loaded(_)));
        assert!(matches!(
            second,
            ModelCatalogEvent::Loaded(catalogs) if catalogs[0].source == "cached catalog"
        ));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        runner.shutdown();
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn hard_refresh_bypasses_and_replaces_a_fresh_cache() {
        let calls = Arc::new(AtomicUsize::new(0));
        let client = LlmClient::with_provider(Arc::new(CountingCatalogProvider {
            calls: Arc::clone(&calls),
        }));
        let path = cache_path();
        let mut runner = ModelCatalogTaskRunner::spawn_with_cache(
            client,
            path.clone(),
            Duration::from_secs(24 * 60 * 60),
        );

        runner.load().unwrap();
        runner.recv_timeout(Duration::from_secs(1)).unwrap();
        runner.refresh().unwrap();
        let refreshed = runner.recv_timeout(Duration::from_secs(1)).unwrap();
        runner.load().unwrap();
        let cached = runner.recv_timeout(Duration::from_secs(1)).unwrap();

        assert!(matches!(
            refreshed,
            ModelCatalogEvent::Loaded(catalogs) if catalogs[0].source == "live provider API"
        ));
        assert!(matches!(
            cached,
            ModelCatalogEvent::Loaded(catalogs) if catalogs[0].source == "cached catalog"
        ));
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        runner.shutdown();
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn expired_cache_is_replaced_from_the_provider() {
        let calls = Arc::new(AtomicUsize::new(0));
        let client = LlmClient::with_provider(Arc::new(CountingCatalogProvider {
            calls: Arc::clone(&calls),
        }));
        let path = cache_path();
        let stale = CachedCatalogs {
            version: CACHE_VERSION,
            fetched_at: 0,
            catalogs: vec![ProviderModels {
                provider: "Stale".into(),
                source: "cached catalog".into(),
                models: Vec::new(),
            }],
        };
        std::fs::write(&path, serde_json::to_vec(&stale).unwrap()).unwrap();
        let mut runner = ModelCatalogTaskRunner::spawn_with_cache(
            client,
            path.clone(),
            Duration::from_secs(24 * 60 * 60),
        );

        runner.load().unwrap();
        let loaded = runner.recv_timeout(Duration::from_secs(1)).unwrap();

        assert!(matches!(
            loaded,
            ModelCatalogEvent::Loaded(catalogs)
                if catalogs[0].provider == "Test" && catalogs[0].source == "live provider API"
        ));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        runner.shutdown();
        let _ = std::fs::remove_file(path);
    }
}
