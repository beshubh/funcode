use super::super::{LlmError, Provider, ProviderRequest, ProviderStream};
use crate::auth::AuthStore;
use futures::{StreamExt, future::BoxFuture};
use rig_core::{
    agent::MultiTurnStreamItem,
    client::CompletionClient,
    providers::chatgpt::{self, ChatGPTAuth},
    streaming::{StreamedAssistantContent, StreamingPrompt},
};

const SYSTEM_INSTRUCTIONS: &str =
    "You are Funcode, a helpful coding assistant. Give clear, accurate, practical answers.";

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
            let agent = client.agent(model).build();
            let stream = agent
                .stream_prompt(request.prompt)
                .with_history(request.history)
                .await;
            let events = stream.filter_map(|item| async move {
                match item {
                    Ok(MultiTurnStreamItem::StreamAssistantItem(
                        StreamedAssistantContent::Text(text),
                    )) => Some(Ok(super::super::ProviderEvent::TextDelta(text.text))),
                    Ok(MultiTurnStreamItem::FinalResponse(response)) => {
                        Some(match response.history().map(<[_]>::to_vec) {
                            Some(history) => Ok(super::super::ProviderEvent::Completed(history)),
                            None => Err(LlmError::Provider(
                                "ChatGPT completed without returning conversation history".into(),
                            )),
                        })
                    }
                    Ok(_) => None,
                    Err(error) => Some(Err(LlmError::Provider(format!(
                        "ChatGPT request failed: {error}"
                    )))),
                }
            });
            Ok(Box::pin(events) as ProviderStream)
        })
    }
}
