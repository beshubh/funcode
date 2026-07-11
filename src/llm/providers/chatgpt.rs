use super::super::{
    ConversationMessage, LlmError, ModelInfo, Provider, ProviderEvent, ProviderModels,
    ProviderRequest, ProviderStream,
};
use crate::auth::AuthStore;
use futures::{Stream, StreamExt, future, future::BoxFuture};
use rig_core::{
    agent::MultiTurnStreamItem,
    client::CompletionClient,
    completion::Message,
    providers::chatgpt::{self, ChatGPTAuth},
    streaming::{StreamedAssistantContent, StreamingPrompt},
};

const SYSTEM_INSTRUCTIONS: &str =
    "You are Funcode, a helpful and fun coding assistant. Give clear, accurate, practical answers.";
const MODELS_URL: &str = "https://chatgpt.com/backend-api/codex/models";
// The backend uses this as a model-catalog schema capability version. Funcode's package is still
// 0.x, which the backend treats as predating picker-visible catalog entries.
const MODEL_CATALOG_CLIENT_VERSION: &str = "1.0.0";

#[derive(serde::Deserialize)]
struct ModelsResponse {
    models: Vec<ChatGptModel>,
}

#[derive(serde::Deserialize)]
struct ChatGptModel {
    slug: String,
    display_name: String,
    description: Option<String>,
    visibility: String,
}

fn parse_models(body: &[u8]) -> Result<ProviderModels, LlmError> {
    let response: ModelsResponse = serde_json::from_slice(body).map_err(|error| {
        LlmError::Provider(format!(
            "ChatGPT returned an invalid model catalog: {error}"
        ))
    })?;
    let models = response
        .models
        .into_iter()
        .filter(|model| model.visibility == "list")
        .map(|model| ModelInfo {
            id: model.slug,
            display_name: model.display_name,
            description: model.description,
        })
        .collect();
    Ok(ProviderModels {
        provider: "ChatGPT".into(),
        source: "live provider API".into(),
        models,
    })
}

pub(in crate::llm) struct ChatGptProvider {
    model: String,
    auth_store: AuthStore,
}

impl ChatGptProvider {
    pub(in crate::llm) fn new(model: String, auth_store: AuthStore) -> Self {
        Self { model, auth_store }
    }
}

impl Provider for ChatGptProvider {
    fn stream(
        &self,
        request: ProviderRequest,
    ) -> BoxFuture<'static, Result<ProviderStream, LlmError>> {
        let model = self.model.clone();
        let auth_store = self.auth_store.clone();
        Box::pin(async move {
            let credentials = auth_store.valid_credentials().await.map_err(|error| {
                if error.to_string().contains("ChatGPT sign-in required") {
                    LlmError::AuthenticationRequired
                } else {
                    LlmError::Provider(format!("could not load ChatGPT credentials: {error}"))
                }
            })?;
            let client = chatgpt::Client::builder()
                .api_key(ChatGPTAuth::AccessToken {
                    access_token: credentials.access_token,
                    account_id: credentials.account_id,
                })
                .default_instructions(SYSTEM_INSTRUCTIONS)
                .originator("funcode")
                .user_agent(format!("funcode/{}", env!("CARGO_PKG_VERSION")))
                .build()
                .map_err(|error| {
                    LlmError::Configuration(format!("could not configure ChatGPT: {error}"))
                })?;
            let ProviderRequest { prompt, history } = request;
            let stream = client
                .agent(model)
                .build()
                .stream_prompt(prompt.clone())
                .with_history(rig_history(&history))
                .await;
            let events = stream.map(|item| match item {
                Ok(MultiTurnStreamItem::StreamAssistantItem(StreamedAssistantContent::Text(
                    text,
                ))) => ChatGptStreamEvent::Text(text.text),
                Ok(MultiTurnStreamItem::StreamAssistantItem(
                    StreamedAssistantContent::ReasoningDelta { reasoning, .. },
                )) => ChatGptStreamEvent::ReasoningSummary(reasoning),
                Ok(MultiTurnStreamItem::FinalResponse(_)) => ChatGptStreamEvent::Finished,
                Ok(_) => ChatGptStreamEvent::Ignored,
                Err(error) => ChatGptStreamEvent::Failed(error.to_string()),
            });
            Ok(stream_events(
                events,
                ConversationAssembler::new(history, prompt),
            ))
        })
    }

    fn list_models(&self) -> BoxFuture<'static, Result<ProviderModels, LlmError>> {
        let auth_store = self.auth_store.clone();
        Box::pin(async move {
            let credentials = auth_store.valid_credentials().await.map_err(auth_error)?;
            let http = reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(15))
                .build()
                .map_err(|error| {
                    LlmError::Configuration(format!(
                        "could not configure ChatGPT model discovery: {error}"
                    ))
                })?;
            let mut request = http
                .get(MODELS_URL)
                .query(&[("client_version", MODEL_CATALOG_CLIENT_VERSION)])
                .bearer_auth(credentials.access_token)
                .header("originator", "funcode")
                .header(
                    "user-agent",
                    format!("funcode/{}", env!("CARGO_PKG_VERSION")),
                );
            if let Some(account_id) = credentials.account_id {
                request = request.header("ChatGPT-Account-Id", account_id);
            }
            let response = request.send().await.map_err(|error| {
                LlmError::Provider(format!("could not list ChatGPT models: {error}"))
            })?;
            let status = response.status();
            if status == reqwest::StatusCode::UNAUTHORIZED {
                return Err(LlmError::AuthenticationRequired);
            }
            if !status.is_success() {
                return Err(LlmError::Provider(format!(
                    "could not list ChatGPT models: server returned {status}"
                )));
            }
            let body = response.bytes().await.map_err(|error| {
                LlmError::Provider(format!("could not read the ChatGPT model catalog: {error}"))
            })?;
            parse_models(&body)
        })
    }
}

fn auth_error(error: anyhow::Error) -> LlmError {
    if error.to_string().contains("ChatGPT sign-in required") {
        LlmError::AuthenticationRequired
    } else {
        LlmError::Provider(format!("could not load ChatGPT credentials: {error}"))
    }
}

enum ChatGptStreamEvent {
    Text(String),
    ReasoningSummary(String),
    Finished,
    Failed(String),
    Ignored,
}

struct ConversationAssembler {
    history: Vec<ConversationMessage>,
    prompt: String,
    response: String,
    terminal: bool,
}

impl ConversationAssembler {
    fn new(history: Vec<ConversationMessage>, prompt: String) -> Self {
        Self {
            history,
            prompt,
            response: String::new(),
            terminal: false,
        }
    }

    fn handle(&mut self, event: ChatGptStreamEvent) -> Option<Result<ProviderEvent, LlmError>> {
        if self.terminal {
            return None;
        }

        match event {
            ChatGptStreamEvent::Text(text) => {
                self.response.push_str(&text);
                Some(Ok(ProviderEvent::TextDelta(text)))
            }
            ChatGptStreamEvent::ReasoningSummary(summary) => {
                Some(Ok(ProviderEvent::ReasoningDelta(summary)))
            }
            ChatGptStreamEvent::Finished => {
                self.terminal = true;
                Some(Ok(ProviderEvent::Completed(completed_history(
                    std::mem::take(&mut self.history),
                    std::mem::take(&mut self.prompt),
                    std::mem::take(&mut self.response),
                ))))
            }
            ChatGptStreamEvent::Failed(message) => {
                self.terminal = true;
                Some(Err(chatgpt_stream_error(message)))
            }
            ChatGptStreamEvent::Ignored => None,
        }
    }
}

fn stream_events<S>(stream: S, assembler: ConversationAssembler) -> ProviderStream
where
    S: Stream<Item = ChatGptStreamEvent> + Send + 'static,
{
    Box::pin(
        stream
            .scan(assembler, |assembler, event| {
                future::ready(Some(assembler.handle(event)))
            })
            .filter_map(future::ready),
    )
}

fn rig_history(history: &[ConversationMessage]) -> Vec<Message> {
    history
        .iter()
        .map(|message| match message {
            ConversationMessage::User(text) => Message::user(text),
            ConversationMessage::Assistant(text) => Message::assistant(text),
        })
        .collect()
}

fn completed_history(
    mut history: Vec<ConversationMessage>,
    prompt: String,
    response: String,
) -> Vec<ConversationMessage> {
    history.push(ConversationMessage::User(prompt));
    history.push(ConversationMessage::Assistant(response));
    history
}

fn chatgpt_stream_error(message: String) -> LlmError {
    let lower = message.to_ascii_lowercase();
    if lower.contains("401")
        || lower.contains("unauthorized")
        || lower.contains("invalid access token")
    {
        LlmError::AuthenticationRequired
    } else {
        LlmError::Provider(format!("ChatGPT request failed: {message}"))
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ChatGptStreamEvent, ConversationAssembler, chatgpt_stream_error, parse_models, rig_history,
        stream_events,
    };
    use crate::llm::{ConversationMessage, LlmError, ProviderEvent};
    use futures::{StreamExt, stream};
    use rig_core::completion::Message;

    #[test]
    fn translates_portable_history_only_inside_the_chatgpt_adapter() {
        let history = vec![
            ConversationMessage::User("first".into()),
            ConversationMessage::Assistant("first response".into()),
        ];

        assert_eq!(
            rig_history(&history),
            vec![Message::user("first"), Message::assistant("first response")]
        );
    }

    #[tokio::test]
    async fn streamed_text_and_final_response_commit_the_portable_transcript() {
        let events = stream_events(
            stream::iter([
                ChatGptStreamEvent::Text("second ".into()),
                ChatGptStreamEvent::Text("response".into()),
                ChatGptStreamEvent::Finished,
            ]),
            ConversationAssembler::new(
                vec![ConversationMessage::User("first".into())],
                "second".into(),
            ),
        )
        .collect::<Vec<_>>()
        .await;

        assert!(matches!(
            &events[0],
            Ok(ProviderEvent::TextDelta(text)) if text == "second "
        ));
        assert!(matches!(
            &events[1],
            Ok(ProviderEvent::TextDelta(text)) if text == "response"
        ));
        assert!(matches!(
            &events[2],
            Ok(ProviderEvent::Completed(history)) if history == &vec![
                ConversationMessage::User("first".into()),
                ConversationMessage::User("second".into()),
                ConversationMessage::Assistant("second response".into()),
            ]
        ));
    }

    #[tokio::test]
    async fn streams_provider_reasoning_summaries_without_putting_them_in_assistant_text() {
        let events = stream_events(
            stream::iter([
                ChatGptStreamEvent::ReasoningSummary("Checking the file list.".into()),
                ChatGptStreamEvent::Text("Done".into()),
                ChatGptStreamEvent::Finished,
            ]),
            ConversationAssembler::new(Vec::new(), "inspect".into()),
        )
        .collect::<Vec<_>>()
        .await;

        assert!(matches!(
            &events[0],
            Ok(ProviderEvent::ReasoningDelta(summary)) if summary == "Checking the file list."
        ));
        assert!(matches!(&events[1], Ok(ProviderEvent::TextDelta(text)) if text == "Done"));
        assert!(matches!(
            &events[2],
            Ok(ProviderEvent::Completed(history))
                if history == &vec![
                    ConversationMessage::User("inspect".into()),
                    ConversationMessage::Assistant("Done".into()),
                ]
        ));
    }

    #[tokio::test]
    async fn stream_failures_do_not_emit_a_conversation_commit() {
        let events = stream_events(
            stream::iter([
                ChatGptStreamEvent::Text("partial".into()),
                ChatGptStreamEvent::Failed("network unavailable".into()),
            ]),
            ConversationAssembler::new(Vec::new(), "prompt".into()),
        )
        .collect::<Vec<_>>()
        .await;

        assert!(matches!(
            &events[0],
            Ok(ProviderEvent::TextDelta(text)) if text == "partial"
        ));
        let error = match &events[1] {
            Err(error) => error,
            Ok(_) => panic!("a failed stream must not emit a completion"),
        };
        assert_eq!(
            error.to_string(),
            "ChatGPT request failed: network unavailable"
        );
        assert_eq!(events.len(), 2);
    }

    #[tokio::test]
    async fn cancelling_a_partial_stream_does_not_emit_a_conversation_commit() {
        let mut events = stream_events(
            stream::iter([ChatGptStreamEvent::Text("partial".into())]).chain(stream::pending()),
            ConversationAssembler::new(Vec::new(), "prompt".into()),
        );

        assert!(matches!(
            events.next().await,
            Some(Ok(ProviderEvent::TextDelta(text))) if text == "partial"
        ));
        drop(events);
    }

    #[tokio::test]
    async fn items_after_a_final_response_are_ignored() {
        let events = stream_events(
            stream::iter([
                ChatGptStreamEvent::Text("complete".into()),
                ChatGptStreamEvent::Finished,
                ChatGptStreamEvent::Text("late text".into()),
                ChatGptStreamEvent::Failed("late error".into()),
            ]),
            ConversationAssembler::new(Vec::new(), "prompt".into()),
        )
        .collect::<Vec<_>>()
        .await;

        assert_eq!(events.len(), 2);
        assert!(matches!(&events[0], Ok(ProviderEvent::TextDelta(text)) if text == "complete"));
        assert!(matches!(&events[1], Ok(ProviderEvent::Completed(_))));
    }

    #[test]
    fn rejected_chatgpt_access_token_requires_authentication() {
        assert_eq!(
            chatgpt_stream_error("HttpError: Invalid status code: 401 Unauthorized".into()),
            LlmError::AuthenticationRequired
        );
    }

    #[test]
    fn ignored_provider_events_do_not_advance_the_transcript() {
        let mut assembler = ConversationAssembler::new(
            vec![ConversationMessage::User("first".into())],
            "second".into(),
        );
        assert!(assembler.handle(ChatGptStreamEvent::Ignored).is_none());
        assert!(matches!(
            assembler.handle(ChatGptStreamEvent::Finished),
            Some(Ok(ProviderEvent::Completed(history))) if history == vec![
                ConversationMessage::User("first".into()),
                ConversationMessage::User("second".into()),
                ConversationMessage::Assistant(String::new()),
            ]
        ));
    }

    #[test]
    fn model_catalog_keeps_only_models_visible_to_users() {
        let catalog = parse_models(
            br#"{
                "models": [
                    {"slug":"gpt-visible","display_name":"GPT Visible","description":"Recommended","visibility":"list"},
                    {"slug":"gpt-hidden","display_name":"GPT Hidden","description":null,"visibility":"hide"}
                ]
            }"#,
        )
        .unwrap();

        assert_eq!(catalog.provider, "ChatGPT");
        assert_eq!(catalog.models.len(), 1);
        assert_eq!(catalog.models[0].id, "gpt-visible");
        assert_eq!(catalog.models[0].display_name, "GPT Visible");
        assert_eq!(
            catalog.models[0].description.as_deref(),
            Some("Recommended")
        );
    }
}
