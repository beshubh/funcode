use futures::{
    StreamExt,
    future::BoxFuture,
    stream::{BoxStream, Stream},
};
use std::{
    fmt,
    pin::Pin,
    sync::{Arc, Mutex},
};

mod providers;

pub(crate) const DEFAULT_MODEL: &str = "gpt-5.4";

pub(crate) type LlmStream = Pin<Box<dyn Stream<Item = Result<LlmEvent, LlmError>> + Send>>;

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum LlmEvent {
    TextDelta(String),
    Completed(ConversationCommit),
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ConversationCommit {
    history: Vec<ConversationMessage>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ConversationMessage {
    User(String),
    Assistant(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LlmError {
    AuthenticationRequired,
    Configuration(String),
    Provider(String),
    Internal(String),
}

impl fmt::Display for LlmError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AuthenticationRequired => {
                formatter.write_str("ChatGPT sign-in required; run /auth")
            }
            Self::Configuration(message) | Self::Provider(message) | Self::Internal(message) => {
                formatter.write_str(message)
            }
        }
    }
}

impl std::error::Error for LlmError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LlmConfig {
    model: String,
}

impl LlmConfig {
    pub(crate) fn from_env() -> Result<Self, LlmError> {
        let model = std::env::var("FUNCODE_MODEL").unwrap_or_else(|_| DEFAULT_MODEL.to_string());
        Self::with_model(model)
    }

    pub(crate) fn with_model(model: impl Into<String>) -> Result<Self, LlmError> {
        let model = model.into().trim().to_owned();
        if model.is_empty() {
            return Err(LlmError::Configuration(
                "FUNCODE_MODEL must not be empty".into(),
            ));
        }
        Ok(Self { model })
    }
}

#[derive(Clone)]
pub(crate) struct ProviderRequest {
    pub(crate) prompt: String,
    pub(crate) history: Vec<ConversationMessage>,
}

pub(crate) enum ProviderEvent {
    TextDelta(String),
    Completed(Vec<ConversationMessage>),
}

pub(crate) type ProviderStream = BoxStream<'static, Result<ProviderEvent, LlmError>>;

pub(crate) trait Provider: Send + Sync {
    fn stream(
        &self,
        request: ProviderRequest,
    ) -> BoxFuture<'static, Result<ProviderStream, LlmError>>;
}

#[derive(Clone)]
pub(crate) struct LlmClient {
    provider: Arc<dyn Provider>,
    history: Arc<Mutex<Vec<ConversationMessage>>>,
}

impl LlmClient {
    pub(crate) fn new(
        config: LlmConfig,
        auth_store: crate::auth::AuthStore,
    ) -> Result<Self, LlmError> {
        Ok(Self::with_provider(Arc::new(
            providers::chatgpt::ChatGptProvider::new(config.model, auth_store),
        )))
    }

    pub(crate) fn with_provider(provider: Arc<dyn Provider>) -> Self {
        Self {
            provider,
            history: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub(crate) async fn stream(&self, prompt: String) -> Result<LlmStream, LlmError> {
        let history = self
            .history
            .lock()
            .map_err(|_| LlmError::Internal("the LLM conversation is unavailable".into()))?
            .clone();
        let stream = self
            .provider
            .stream(ProviderRequest { prompt, history })
            .await?;
        Ok(Box::pin(stream.map(move |event| match event? {
            ProviderEvent::TextDelta(text) => Ok(LlmEvent::TextDelta(text)),
            ProviderEvent::Completed(history) => {
                Ok(LlmEvent::Completed(ConversationCommit { history }))
            }
        })))
    }

    pub(crate) fn commit(&self, commit: ConversationCommit) -> Result<(), LlmError> {
        *self
            .history
            .lock()
            .map_err(|_| LlmError::Internal("the LLM conversation is unavailable".into()))? =
            commit.history;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ConversationMessage, LlmClient, LlmError, LlmEvent, Provider, ProviderEvent,
        ProviderRequest, ProviderStream,
    };
    use futures::{StreamExt, future::BoxFuture, stream};
    use std::{
        collections::VecDeque,
        sync::{Arc, Mutex},
    };

    struct StreamingProvider;

    impl Provider for StreamingProvider {
        fn stream(
            &self,
            request: ProviderRequest,
        ) -> BoxFuture<'static, Result<ProviderStream, LlmError>> {
            Box::pin(async move {
                let history = vec![
                    ConversationMessage::User(request.prompt),
                    ConversationMessage::Assistant("hello world".into()),
                ];
                Ok(Box::pin(stream::iter([
                    Ok(ProviderEvent::TextDelta("hello ".into())),
                    Ok(ProviderEvent::TextDelta("world".into())),
                    Ok(ProviderEvent::Completed(history)),
                ])) as ProviderStream)
            })
        }
    }

    #[tokio::test]
    async fn streams_provider_text_and_completion_to_the_caller() {
        let client = LlmClient::with_provider(Arc::new(StreamingProvider));

        let events = client
            .stream("hello".into())
            .await
            .unwrap()
            .collect::<Vec<_>>()
            .await;

        assert_eq!(events[0], Ok(LlmEvent::TextDelta("hello ".into())));
        assert_eq!(events[1], Ok(LlmEvent::TextDelta("world".into())));
        assert!(matches!(events[2], Ok(LlmEvent::Completed(_))));
    }

    struct RecordingProvider {
        requests: Arc<Mutex<Vec<ProviderRequest>>>,
        responses: Mutex<VecDeque<Vec<Result<ProviderEvent, LlmError>>>>,
    }

    impl Provider for RecordingProvider {
        fn stream(
            &self,
            request: ProviderRequest,
        ) -> BoxFuture<'static, Result<ProviderStream, LlmError>> {
            self.requests.lock().unwrap().push(request);
            let response = self.responses.lock().unwrap().pop_front().unwrap();
            Box::pin(async move { Ok(Box::pin(stream::iter(response)) as ProviderStream) })
        }
    }

    #[tokio::test]
    async fn completed_history_is_sent_with_the_next_prompt() {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let first_history = vec![
            ConversationMessage::User("first".into()),
            ConversationMessage::Assistant("first response".into()),
        ];
        let provider = RecordingProvider {
            requests: Arc::clone(&requests),
            responses: Mutex::new(VecDeque::from([
                vec![Ok(ProviderEvent::Completed(first_history.clone()))],
                vec![Ok(ProviderEvent::Completed(Vec::new()))],
            ])),
        };
        let client = LlmClient::with_provider(Arc::new(provider));

        let events = client
            .stream("first".into())
            .await
            .unwrap()
            .collect::<Vec<_>>()
            .await;
        let commit = events
            .into_iter()
            .find_map(|event| match event.unwrap() {
                LlmEvent::Completed(commit) => Some(commit),
                LlmEvent::TextDelta(_) => None,
            })
            .unwrap();
        client.commit(commit).unwrap();
        client
            .stream("second".into())
            .await
            .unwrap()
            .collect::<Vec<_>>()
            .await;

        let requests = requests.lock().unwrap();
        assert_eq!(requests[1].prompt, "second");
        assert_eq!(requests[1].history, first_history);
    }

    #[tokio::test]
    async fn failed_responses_are_not_added_to_conversation_history() {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let provider = RecordingProvider {
            requests: Arc::clone(&requests),
            responses: Mutex::new(VecDeque::from([
                vec![
                    Ok(ProviderEvent::TextDelta("partial".into())),
                    Err(LlmError::Provider("request failed".into())),
                ],
                vec![Ok(ProviderEvent::Completed(Vec::new()))],
            ])),
        };
        let client = LlmClient::with_provider(Arc::new(provider));

        client
            .stream("failed".into())
            .await
            .unwrap()
            .collect::<Vec<_>>()
            .await;
        client
            .stream("next".into())
            .await
            .unwrap()
            .collect::<Vec<_>>()
            .await;

        assert!(requests.lock().unwrap()[1].history.is_empty());
    }

    #[test]
    fn empty_model_configuration_is_rejected() {
        let error = super::LlmConfig::with_model("  ").unwrap_err();
        assert_eq!(
            error,
            LlmError::Configuration("FUNCODE_MODEL must not be empty".into())
        );
    }

    #[test]
    fn model_configuration_trims_whitespace() {
        let config = super::LlmConfig::with_model(" gpt-5.4 ").unwrap();
        assert_eq!(config.model, "gpt-5.4");
    }

    #[tokio::test]
    async fn missing_chatgpt_credentials_require_sign_in() {
        let path = std::env::temp_dir().join(format!(
            "funcode-missing-auth-{}-{}.json",
            std::process::id(),
            rand::random::<u64>()
        ));
        let client = LlmClient::new(
            super::LlmConfig::with_model(super::DEFAULT_MODEL).unwrap(),
            crate::auth::AuthStore::at(path),
        )
        .unwrap();

        let error = match client.stream("hello".into()).await {
            Ok(_) => panic!("missing credentials should fail before streaming"),
            Err(error) => error,
        };

        assert_eq!(error, LlmError::AuthenticationRequired);
        assert_eq!(error.to_string(), "ChatGPT sign-in required; run /auth");
    }
}
