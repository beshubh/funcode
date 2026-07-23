use super::super::{
    ConversationMessage, LlmError, ModelInfo, Provider, ProviderEvent, ProviderModels,
    ProviderRequest, ProviderStream,
};
use crate::{
    auth::AuthStore,
    tools::{AgentTool, ToolSession, ToolSpec},
    usage::TokenUsage,
};
use futures::{Stream, StreamExt, future, future::BoxFuture};
use rig_core::{
    OneOrMany,
    agent::MultiTurnStreamItem,
    client::CompletionClient,
    completion::{CompletionRequest as RigCompletionRequest, Message, ToolDefinition},
    providers::chatgpt::{self, ChatGPTAuth},
    providers::openai::responses_api::{CompletionRequest as ResponsesRequest, Include},
    streaming::{StreamedAssistantContent, StreamingPrompt},
    tool::{ToolDyn, ToolError},
    wasm_compat::WasmBoxedFuture,
};
use std::sync::{Arc, LazyLock};

const BUILD_MODE_SYSTEM_INSTRUCTIONS: &str = include_str!("prompts/build.md");
const PLAN_MODE_SYSTEM_INSTRUCTIONS: &str = include_str!("prompts/plan.md");
static SYSTEM_INSTRUCTIONS: LazyLock<String> = LazyLock::new(|| {
    let build = BUILD_MODE_SYSTEM_INSTRUCTIONS
        .strip_suffix('\n')
        .unwrap_or(BUILD_MODE_SYSTEM_INSTRUCTIONS);
    let plan = PLAN_MODE_SYSTEM_INSTRUCTIONS
        .strip_suffix('\n')
        .unwrap_or(PLAN_MODE_SYSTEM_INSTRUCTIONS);
    format!("{build} {plan}")
});
const MODELS_URL: &str = "https://chatgpt.com/backend-api/codex/models";
// The backend uses this as a model-catalog schema capability version. Funcode's package is still
// 0.x, which the backend treats as predating picker-visible catalog entries.
const MODEL_CATALOG_CLIENT_VERSION: &str = "1.0.0";
const MAX_MULTI_TURN_LIMIT: usize = 1000;

pub(in crate::llm) fn serialized_request_bytes(
    model: &str,
    prompt: &str,
    history: &[ConversationMessage],
    tools: &[ToolSpec],
) -> Result<usize, LlmError> {
    let mut messages = rig_history(history);
    messages.push(Message::user(prompt));
    let completion = RigCompletionRequest {
        model: None,
        preamble: None,
        chat_history: OneOrMany::many(messages).map_err(|error| {
            LlmError::Internal(format!("could not serialize the ChatGPT request: {error}"))
        })?,
        documents: Vec::new(),
        tools: tools
            .iter()
            .map(|tool| ToolDefinition {
                name: tool.name.to_owned(),
                description: tool.description.clone(),
                parameters: tool.parameters.clone(),
            })
            .collect(),
        temperature: None,
        max_tokens: None,
        tool_choice: None,
        additional_params: None,
        output_schema: None,
    };
    let mut request =
        ResponsesRequest::try_from((model.to_owned(), completion)).map_err(|error| {
            LlmError::Internal(format!("could not serialize the ChatGPT request: {error}"))
        })?;

    // Keep this in lockstep with `ChatGptProvider::stream` and Rig's ChatGPT
    // request normalization. This is the exact body passed to `serde_json::to_vec`
    // by the provider before the HTTP request is sent.
    request.instructions = Some(SYSTEM_INSTRUCTIONS.clone());
    request.temperature = None;
    request.max_output_tokens = None;
    request.stream = Some(true);
    let include = request
        .additional_parameters
        .include
        .get_or_insert_with(Vec::new);
    if !include
        .iter()
        .any(|item| matches!(item, Include::ReasoningEncryptedContent))
    {
        include.push(Include::ReasoningEncryptedContent);
    }
    request.additional_parameters.background = None;
    request.additional_parameters.metadata.clear();
    request.additional_parameters.parallel_tool_calls = None;
    request.additional_parameters.service_tier = None;
    request.additional_parameters.store = Some(false);
    request.additional_parameters.text = None;
    request.additional_parameters.top_p = None;
    request.additional_parameters.user = None;

    serde_json::to_vec(&request)
        .map(|body| body.len())
        .map_err(|error| {
            LlmError::Internal(format!("could not serialize the ChatGPT request: {error}"))
        })
}

#[derive(serde::Deserialize)]
struct ModelsResponse {
    models: Vec<ChatGptModel>,
}

#[derive(serde::Deserialize)]
struct ChatGptModel {
    slug: String,
    display_name: String,
    visibility: String,
    #[serde(default)]
    context_window: Option<u64>,
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
            context_window: model.context_window,
        })
        .collect();
    Ok(ProviderModels {
        provider: "ChatGPT".into(),
        source: "live provider API".into(),
        models,
    })
}

pub(in crate::llm) struct ChatGptProvider {
    auth_store: AuthStore,
}

impl ChatGptProvider {
    pub(in crate::llm) fn new(auth_store: AuthStore) -> Self {
        Self { auth_store }
    }
}

impl Provider for ChatGptProvider {
    fn stream(
        &self,
        request: ProviderRequest,
    ) -> BoxFuture<'static, Result<ProviderStream, LlmError>> {
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
                .default_instructions(SYSTEM_INSTRUCTIONS.as_str())
                .originator("funcode")
                .user_agent(format!("funcode/{}", env!("CARGO_PKG_VERSION")))
                .build()
                .map_err(|error| {
                    LlmError::Configuration(format!("could not configure ChatGPT: {error}"))
                })?;
            let ProviderRequest {
                model,
                prompt,
                history,
                tools,
            } = request;
            let rig_history = rig_history(&history);
            let stream = if let Some(session) = tools {
                let tools = session
                    .tools()
                    .into_iter()
                    .map(|tool| {
                        Box::new(RigToolAdapter {
                            tool,
                            session: session.clone(),
                        }) as Box<dyn ToolDyn>
                    })
                    .collect();
                client
                    .agent(model)
                    .tools(tools)
                    .build()
                    .stream_prompt(prompt.clone())
                    .with_history(&rig_history)
                    .multi_turn(MAX_MULTI_TURN_LIMIT)
                    .await
            } else {
                client
                    .agent(model)
                    .build()
                    .stream_prompt(prompt.clone())
                    .with_history(&rig_history)
                    .await
            };
            let events = stream.map(|item| match item {
                Ok(MultiTurnStreamItem::StreamAssistantItem(StreamedAssistantContent::Text(
                    text,
                ))) => ChatGptStreamEvent::Text(text.text),
                Ok(MultiTurnStreamItem::StreamAssistantItem(
                    StreamedAssistantContent::ReasoningDelta { reasoning, .. },
                )) => ChatGptStreamEvent::ReasoningSummary(reasoning),
                Ok(MultiTurnStreamItem::CompletionCall(call)) => {
                    ChatGptStreamEvent::Usage(TokenUsage {
                        input_tokens: call.usage.input_tokens,
                        output_tokens: call.usage.output_tokens,
                        total_tokens: call.usage.total_tokens,
                        reasoning_tokens: call.usage.reasoning_tokens,
                    })
                }
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

struct RigToolAdapter {
    tool: Arc<dyn AgentTool>,
    session: ToolSession,
}

impl ToolDyn for RigToolAdapter {
    fn name(&self) -> String {
        self.session.spec(self.tool.as_ref()).name.to_owned()
    }

    fn definition<'a>(
        &'a self,
        _prompt: String,
    ) -> WasmBoxedFuture<'a, rig_core::completion::ToolDefinition> {
        Box::pin(async move {
            let spec = self.session.spec(self.tool.as_ref());
            rig_core::completion::ToolDefinition {
                name: spec.name.to_owned(),
                description: spec.description,
                parameters: spec.parameters,
            }
        })
    }

    fn call<'a>(&'a self, args: String) -> WasmBoxedFuture<'a, Result<String, ToolError>> {
        Box::pin(async move {
            self.session
                .execute(Arc::clone(&self.tool), args)
                .await
                .map_err(|error| ToolError::ToolCallError(Box::new(error)))
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
    Usage(TokenUsage),
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
            ChatGptStreamEvent::Usage(usage) => Some(Ok(ProviderEvent::Usage(usage))),
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
        BUILD_MODE_SYSTEM_INSTRUCTIONS, ChatGptStreamEvent, ConversationAssembler,
        PLAN_MODE_SYSTEM_INSTRUCTIONS, RigToolAdapter, SYSTEM_INSTRUCTIONS, chatgpt_stream_error,
        parse_models, rig_history, serialized_request_bytes, stream_events,
    };
    use crate::llm::{ConversationMessage, LlmError, ProviderEvent};
    use crate::session::SessionMode;
    use crate::tools::{ToolRegistry, ToolSession};
    use futures::{StreamExt, stream};
    use rig_core::completion::Message;
    use rig_core::tool::ToolDyn;
    use std::{fs, sync::Arc};
    use tokio::sync::mpsc;

    #[test]
    fn embedded_mode_system_instructions_preserve_the_existing_prompt() {
        assert_eq!(
            BUILD_MODE_SYSTEM_INSTRUCTIONS,
            "You are Funcode, a helpful and fun coding assistant. Give clear, accurate, practical answers. Build mode provides read_file, search_files, edit_file, and terminal. Inspect before editing, keep changes scoped to the request, and verify changes with relevant commands before answering.\n"
        );
        assert_eq!(
            PLAN_MODE_SYSTEM_INSTRUCTIONS,
            "Plan mode omits edit_file and permits terminal only for non-mutating inspection.\n"
        );
        assert_eq!(
            SYSTEM_INSTRUCTIONS.as_str(),
            "You are Funcode, a helpful and fun coding assistant. Give clear, accurate, practical answers. Build mode provides read_file, search_files, edit_file, and terminal. Inspect before editing, keep changes scoped to the request, and verify changes with relevant commands before answering. Plan mode omits edit_file and permits terminal only for non-mutating inspection."
        );
    }

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

    #[test]
    fn serialized_request_size_includes_json_escaping_history_instructions_and_tools() {
        let prompt = "quoted \" text\nwith control \u{1}";
        let history = vec![
            ConversationMessage::User("earlier".into()),
            ConversationMessage::Assistant("answer".into()),
        ];
        let build_tools = ToolRegistry::for_mode(SessionMode::Build).specs(SessionMode::Build);
        let plan_tools = ToolRegistry::for_mode(SessionMode::Plan).specs(SessionMode::Plan);

        let build_bytes =
            serialized_request_bytes("gpt-5.4", prompt, &history, &build_tools).unwrap();
        let plan_bytes = serialized_request_bytes("gpt-5.4", prompt, &[], &plan_tools).unwrap();

        assert!(build_bytes > prompt.len());
        assert!(build_bytes > plan_bytes);
        assert!(
            build_bytes > serialized_request_bytes("gpt-5.4", prompt, &[], &build_tools).unwrap()
        );
    }

    #[tokio::test]
    async fn rig_adapter_exposes_portable_tools_and_plain_text_results() {
        let root = tempfile::tempdir().expect("temporary workspace should be created");
        fs::write(root.path().join("value.txt"), "hello\n").expect("fixture should be written");
        let (events, _event_rx) = mpsc::unbounded_channel();
        let session = ToolSession::new(root.path().to_owned(), SessionMode::Plan, events, 4)
            .expect("tool session should be created");
        let tool = session
            .tools()
            .into_iter()
            .find(|tool| session.spec(tool.as_ref()).name == "read_file")
            .expect("read tool should be registered");
        let adapter = RigToolAdapter {
            tool: Arc::clone(&tool),
            session,
        };

        let definition = adapter.definition(String::new()).await;
        let output = adapter
            .call(r#"{"path":"value.txt"}"#.into())
            .await
            .expect("tool result should be returned to Rig");

        assert_eq!(definition.name, "read_file");
        assert_eq!(output, "     1\thello");
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
    async fn forwards_completion_usage_before_the_final_conversation_commit() {
        let usage = crate::usage::TokenUsage {
            input_tokens: 120,
            output_tokens: 30,
            total_tokens: 150,
            reasoning_tokens: 0,
        };
        let events = stream_events(
            stream::iter([
                ChatGptStreamEvent::Usage(usage),
                ChatGptStreamEvent::Finished,
            ]),
            ConversationAssembler::new(Vec::new(), "prompt".into()),
        )
        .collect::<Vec<_>>()
        .await;

        assert!(matches!(
            events[0],
            Ok(ProviderEvent::Usage(reported)) if reported == usage
        ));
        assert!(matches!(events[1], Ok(ProviderEvent::Completed(_))));
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
                    {"slug":"gpt-visible","display_name":"GPT Visible","description":"Recommended","visibility":"list","context_window":128000},
                    {"slug":"gpt-hidden","display_name":"GPT Hidden","description":null,"visibility":"hide"}
                ]
            }"#,
        )
        .unwrap();

        assert_eq!(catalog.provider, "ChatGPT");
        assert_eq!(catalog.models.len(), 1);
        assert_eq!(catalog.models[0].id, "gpt-visible");
        assert_eq!(catalog.models[0].display_name, "GPT Visible");
        assert_eq!(catalog.models[0].context_window, Some(128_000));
    }
}
