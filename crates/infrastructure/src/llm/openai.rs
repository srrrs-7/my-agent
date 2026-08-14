//! OpenAI `/chat/completions` client.
//!
//! Deliberately the *compatible* dialect rather than "OpenAI": Ollama, vLLM,
//! LM Studio, llama.cpp, OpenRouter, Groq and Together all expose this
//! endpoint, so one adapter covers local and cloud deployments alike. The
//! places where implementations disagree are handled defensively:
//!
//! * `tool_calls[].function.arguments` is a JSON *string* per the spec, but
//!   several servers send an object. Both are accepted.
//! * `finish_reason` is often `"stop"` even when tool calls are present, so the
//!   stop reason is derived from the content, not from the label.
//! * `content` may be `null`, a string, or an array of parts.
//! * newer OpenAI models require `max_completion_tokens` instead of
//!   `max_tokens`; the field name is configurable.

use std::collections::{BTreeMap, VecDeque};
use std::time::Duration;

use agent_domain::error::LlmError;
use agent_domain::model::llm::{
    ChatRequest, ChatResponse, ModelId, ProviderCapabilities, ProviderId, StopReason, TokenUsage,
};
use agent_domain::model::message::{ContentBlock, Message, Role};
use agent_domain::model::tool::{ToolCall, ToolCallId, ToolName};
use agent_domain::ports::llm::{ChatStream, LlmProvider, StreamEvent};
use agent_domain::text::clip;
use async_trait::async_trait;
use futures::StreamExt as _;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tracing::debug;

use super::http;
use super::sse::SseFraming;

pub struct OpenAiCompatibleProvider {
    id: ProviderId,
    client: reqwest::Client,
    base_url: String,
    api_key: Option<String>,
    default_model: ModelId,
    max_tokens_field: String,
    timeout: Duration,
}

impl OpenAiCompatibleProvider {
    pub fn new(
        id: ProviderId,
        base_url: impl Into<String>,
        api_key: Option<String>,
        default_model: ModelId,
        max_tokens_field: impl Into<String>,
        timeout: Duration,
    ) -> Result<Self, LlmError> {
        Ok(Self {
            id,
            client: http::build_client(timeout)?,
            base_url: base_url.into().trim_end_matches('/').to_string(),
            api_key,
            default_model,
            max_tokens_field: max_tokens_field.into(),
            timeout,
        })
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }
}

#[async_trait]
impl LlmProvider for OpenAiCompatibleProvider {
    fn id(&self) -> ProviderId {
        self.id.clone()
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            supports_tools: true,
            supports_streaming: true,
            supports_system_prompt: true,
            max_context_tokens: None,
        }
    }

    async fn chat(&self, request: ChatRequest) -> Result<ChatResponse, LlmError> {
        let model = request
            .model
            .clone()
            .unwrap_or_else(|| self.default_model.clone());
        let builder = self.request_builder(&request, &model, false);

        let text = http::send(builder, &self.base_url, self.timeout).await?;

        let parsed: WireResponse = serde_json::from_str(&text).map_err(|error| {
            LlmError::InvalidResponse(format!("{error} (body: {})", clip(&text, 300)))
        })?;

        self.decode(parsed, model)
    }

    async fn chat_stream(&self, request: ChatRequest) -> Result<ChatStream, LlmError> {
        let model = request
            .model
            .clone()
            .unwrap_or_else(|| self.default_model.clone());
        let builder = self.request_builder(&request, &model, true);

        // A failure here (bad status, unreachable host) happens before any
        // event was produced, so the retry decorator may safely retry it.
        let bytes = http::send_streaming(builder, &self.base_url, self.timeout).await?;

        Ok(stream_events(bytes, self.id.clone(), model))
    }
}

impl OpenAiCompatibleProvider {
    /// Builds the request both entry points share; only `stream` differs.
    fn request_builder(
        &self,
        request: &ChatRequest,
        model: &ModelId,
        stream: bool,
    ) -> reqwest::RequestBuilder {
        let url = format!("{}/chat/completions", self.base_url);

        let mut messages = Vec::with_capacity(request.messages.len() + 1);
        if let Some(system) = &request.system {
            messages.push(WireMessage::simple("system", system.clone()));
        }
        for message in &request.messages {
            append_message(&mut messages, message);
        }

        let mut max_tokens = BTreeMap::new();
        if let Some(limit) = request.params.max_tokens {
            max_tokens.insert(self.max_tokens_field.clone(), limit);
        }

        let body = WireRequest {
            model: model.as_str(),
            messages,
            tools: request
                .tools
                .iter()
                .map(WireTool::from_definition)
                .collect(),
            temperature: request.params.temperature,
            top_p: request.params.top_p,
            stop: request.params.stop_sequences.clone(),
            max_tokens,
            stream,
            // Without this the final usage chunk is omitted by most servers.
            // Tolerated by the ones that predate it via the AGENT_STREAM=false
            // escape hatch.
            stream_options: stream.then_some(WireStreamOptions {
                include_usage: true,
            }),
        };

        let mut builder = self.client.post(&url).json(&body);
        if let Some(key) = &self.api_key {
            builder = builder.bearer_auth(key);
        }
        builder
    }

    fn decode(
        &self,
        response: WireResponse,
        requested_model: ModelId,
    ) -> Result<ChatResponse, LlmError> {
        decode_response(&self.id, response, requested_model)
    }
}

/// Decodes a complete (or stream-assembled) response. Free-standing so the
/// `'static` event stream can decode without borrowing the provider.
fn decode_response(
    provider: &ProviderId,
    response: WireResponse,
    requested_model: ModelId,
) -> Result<ChatResponse, LlmError> {
    {
        let choice = response.choices.into_iter().next().ok_or_else(|| {
            LlmError::InvalidResponse("the response contained no choices".to_string())
        })?;

        let mut content = Vec::new();
        if let Some(text) = content_to_text(choice.message.content) {
            if !text.trim().is_empty() {
                content.push(ContentBlock::text(text));
            }
        }

        for (index, wire_call) in choice.message.tool_calls.into_iter().enumerate() {
            let name = ToolName::new(wire_call.function.name.clone()).map_err(|error| {
                LlmError::InvalidResponse(format!(
                    "the model asked for an unusable tool name `{}`: {error}",
                    wire_call.function.name
                ))
            })?;
            let id = ToolCallId::new(
                wire_call
                    .id
                    .filter(|id| !id.is_empty())
                    .unwrap_or_else(|| format!("call_{index}")),
            );
            content.push(ContentBlock::ToolCall(ToolCall::new(
                id,
                name,
                normalize_arguments(wire_call.function.arguments)?,
            )));
        }

        let has_tool_calls = content
            .iter()
            .any(|block| matches!(block, ContentBlock::ToolCall(_)));

        let stop_reason = if has_tool_calls {
            StopReason::ToolUse
        } else {
            match choice.finish_reason.as_deref() {
                Some("stop") | None => StopReason::EndTurn,
                Some("length") => StopReason::MaxTokens,
                Some("tool_calls") | Some("function_call") => StopReason::ToolUse,
                Some(other) => StopReason::Other(other.to_string()),
            }
        };

        let usage = response
            .usage
            .map(|usage| TokenUsage {
                input_tokens: usage.prompt_tokens.unwrap_or_default(),
                output_tokens: usage.completion_tokens.unwrap_or_default(),
            })
            .unwrap_or_default();

        debug!(provider = %provider, tool_calls = has_tool_calls, "chat completion decoded");

        Ok(ChatResponse {
            message: Message::assistant(content),
            stop_reason,
            usage,
            model: response.model.map(ModelId::new).unwrap_or(requested_model),
            provider: provider.clone(),
        })
    }
}

// --- streaming ---------------------------------------------------------------

/// Turns the SSE byte stream into the domain's [`StreamEvent`] protocol.
///
/// Chunks are absorbed into a [`StreamAccumulator`]; prose surfaces
/// immediately as [`StreamEvent::TextDelta`], tool-call fragments only
/// accumulate. When the server signals completion (`[DONE]`, or EOF after a
/// `finish_reason`), the accumulated state is decoded through the same
/// [`decode_response`] path the non-streaming call uses - so a streamed turn
/// and a plain turn obey identical invariants, tool-call aggregation included.
fn stream_events(
    bytes: http::ByteStream,
    provider: ProviderId,
    requested_model: ModelId,
) -> ChatStream {
    struct State {
        bytes: http::ByteStream,
        framing: SseFraming,
        /// `None` once the final response has been assembled.
        accumulator: Option<StreamAccumulator>,
        pending: VecDeque<Result<StreamEvent, LlmError>>,
        saw_done: bool,
        finished: bool,
        provider: ProviderId,
        requested_model: Option<ModelId>,
    }

    impl State {
        /// Assembles the final [`StreamEvent::Completed`] (or the error that
        /// explains why there is none) and queues it.
        fn finalize(&mut self) {
            let Some(accumulator) = self.accumulator.take() else {
                return;
            };
            let event = if self.saw_done || accumulator.is_complete() {
                let model = self
                    .requested_model
                    .take()
                    .expect("requested_model is taken exactly once, with the accumulator");
                decode_response(&self.provider, accumulator.into_response(), model)
                    .map(StreamEvent::Completed)
            } else {
                // EOF without a completion marker: the connection broke.
                // Resuming a half-delivered stream is explicitly out of scope.
                Err(LlmError::InvalidResponse(
                    "the stream ended before the response completed".to_string(),
                ))
            };
            self.pending.push_back(event);
        }
    }

    let state = State {
        bytes,
        framing: SseFraming::new(),
        accumulator: Some(StreamAccumulator::default()),
        pending: VecDeque::new(),
        saw_done: false,
        finished: false,
        provider,
        requested_model: Some(requested_model),
    };

    Box::pin(futures::stream::unfold(state, |mut state| async move {
        loop {
            if let Some(event) = state.pending.pop_front() {
                // `Completed` and errors are terminal: nothing may follow them.
                if matches!(event, Err(_) | Ok(StreamEvent::Completed(_))) {
                    state.finished = true;
                }
                return Some((event, state));
            }
            if state.finished {
                return None;
            }

            match state.bytes.next().await {
                Some(Ok(chunk)) => {
                    for payload in state.framing.feed(&chunk) {
                        if state.accumulator.is_none() {
                            // Trailing data after completion; ignore.
                            continue;
                        }
                        if payload == "[DONE]" {
                            state.saw_done = true;
                            state.finalize();
                            continue;
                        }
                        match serde_json::from_str::<WireStreamChunk>(&payload) {
                            Ok(parsed) => {
                                let delta = state
                                    .accumulator
                                    .as_mut()
                                    .expect("checked above")
                                    .absorb(parsed);
                                if let Some(delta) = delta {
                                    state.pending.push_back(Ok(StreamEvent::TextDelta(delta)));
                                }
                            }
                            Err(error) => {
                                state
                                    .pending
                                    .push_back(Err(LlmError::InvalidResponse(format!(
                                        "undecodable stream chunk: {error} (payload: {})",
                                        clip(&payload, 200)
                                    ))));
                            }
                        }
                    }
                }
                Some(Err(error)) => {
                    state.pending.push_back(Err(error));
                }
                None => {
                    state.finalize();
                    if state.pending.is_empty() {
                        // Finalize was a no-op (already completed earlier).
                        return None;
                    }
                }
            }
        }
    }))
}

/// Reassembles a chat completion from stream chunks.
///
/// Tool-call fragments are keyed by the wire `index` and their `arguments`
/// strings concatenated; nothing is surfaced until [`Self::into_response`], so
/// a partially-received call can never leak out.
#[derive(Debug, Default)]
struct StreamAccumulator {
    model: Option<String>,
    text: String,
    calls: BTreeMap<u32, PartialToolCall>,
    finish_reason: Option<String>,
    usage: Option<WireUsage>,
}

#[derive(Debug, Default)]
struct PartialToolCall {
    id: Option<String>,
    name: String,
    arguments: String,
}

impl StreamAccumulator {
    /// Absorbs one chunk and returns any prose to surface as a delta.
    fn absorb(&mut self, chunk: WireStreamChunk) -> Option<String> {
        if self.model.is_none() {
            self.model = chunk.model;
        }
        if chunk.usage.is_some() {
            self.usage = chunk.usage;
        }

        let mut delta_text: Option<String> = None;
        for choice in chunk.choices {
            if choice.finish_reason.is_some() {
                self.finish_reason = choice.finish_reason;
            }
            if let Some(content) = choice.delta.content.as_ref().and_then(Value::as_str) {
                if !content.is_empty() {
                    self.text.push_str(content);
                    delta_text.get_or_insert_default().push_str(content);
                }
            }
            for call in choice.delta.tool_calls {
                let entry = self.calls.entry(call.index.unwrap_or(0)).or_default();
                if entry.id.is_none() {
                    entry.id = call.id.filter(|id| !id.is_empty());
                }
                if let Some(name) = call.function.name {
                    entry.name.push_str(&name);
                }
                if let Some(arguments) = call.function.arguments {
                    entry.arguments.push_str(&arguments);
                }
            }
        }
        delta_text
    }

    /// Whether the server marked the turn as finished.
    fn is_complete(&self) -> bool {
        self.finish_reason.is_some()
    }

    /// Synthesizes the non-streaming wire shape so [`decode_response`] applies
    /// unchanged - id fallbacks, argument parsing, stop-reason policy and all.
    fn into_response(self) -> WireResponse {
        WireResponse {
            model: self.model,
            choices: vec![WireChoice {
                message: WireResponseMessage {
                    content: Some(Value::String(self.text)),
                    tool_calls: self
                        .calls
                        .into_values()
                        .map(|call| WireResponseToolCall {
                            id: call.id,
                            function: WireResponseFunction {
                                name: call.name,
                                arguments: Value::String(call.arguments),
                            },
                        })
                        .collect(),
                },
                finish_reason: self.finish_reason,
            }],
            usage: self.usage,
        }
    }
}

// --- outbound mapping --------------------------------------------------------

fn append_message(messages: &mut Vec<WireMessage>, message: &Message) {
    match message.role {
        Role::System => messages.push(WireMessage::simple("system", message.text())),
        Role::User => messages.push(WireMessage::simple("user", message.text())),
        Role::Assistant => {
            let tool_calls: Vec<WireToolCall> = message
                .tool_calls()
                .map(|call| WireToolCall {
                    id: call.id.to_string(),
                    kind: "function",
                    function: WireFunctionCall {
                        name: call.name.to_string(),
                        // The spec says this is a string, and strict servers
                        // reject an object here.
                        arguments: call.arguments.to_string(),
                    },
                })
                .collect();

            messages.push(WireMessage {
                role: "assistant",
                content: Some(message.text()),
                tool_calls: if tool_calls.is_empty() {
                    None
                } else {
                    Some(tool_calls)
                },
                tool_call_id: None,
            });
        }
        Role::Tool => {
            // Each tool result becomes its own `role: "tool"` message.
            for block in &message.content {
                if let ContentBlock::ToolResult(result) = block {
                    messages.push(WireMessage {
                        role: "tool",
                        content: Some(result.content.clone()),
                        tool_calls: None,
                        tool_call_id: Some(result.call_id.to_string()),
                    });
                }
            }
        }
    }
}

fn normalize_arguments(raw: Value) -> Result<Value, LlmError> {
    match raw {
        Value::String(text) if text.trim().is_empty() => Ok(Value::Object(Default::default())),
        Value::String(text) => serde_json::from_str(&text).map_err(|error| {
            LlmError::InvalidResponse(format!(
                "tool arguments were not valid JSON: {error} (raw: {})",
                clip(&text, 200)
            ))
        }),
        Value::Null => Ok(Value::Object(Default::default())),
        other => Ok(other),
    }
}

fn content_to_text(content: Option<Value>) -> Option<String> {
    match content? {
        Value::String(text) => Some(text),
        // Some gateways return the multimodal array form even for plain text.
        Value::Array(parts) => {
            let text: Vec<String> = parts
                .into_iter()
                .filter_map(|part| part.get("text").and_then(Value::as_str).map(str::to_string))
                .collect();
            Some(text.join(""))
        }
        Value::Null => None,
        other => Some(other.to_string()),
    }
}

// --- wire types --------------------------------------------------------------

#[derive(Debug, Serialize)]
struct WireRequest<'a> {
    model: &'a str,
    messages: Vec<WireMessage>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<WireTool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    top_p: Option<f32>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    stop: Vec<String>,
    /// Flattened so the key can be `max_tokens` or `max_completion_tokens`.
    #[serde(flatten)]
    max_tokens: BTreeMap<String, u32>,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    stream_options: Option<WireStreamOptions>,
}

#[derive(Debug, Serialize)]
struct WireStreamOptions {
    include_usage: bool,
}

// --- streaming wire types ----------------------------------------------------

#[derive(Debug, Deserialize)]
struct WireStreamChunk {
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    choices: Vec<WireStreamChoice>,
    #[serde(default)]
    usage: Option<WireUsage>,
}

#[derive(Debug, Deserialize)]
struct WireStreamChoice {
    #[serde(default)]
    delta: WireStreamDelta,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct WireStreamDelta {
    #[serde(default)]
    content: Option<Value>,
    #[serde(default)]
    tool_calls: Vec<WireStreamToolCall>,
}

#[derive(Debug, Deserialize)]
struct WireStreamToolCall {
    #[serde(default)]
    index: Option<u32>,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    function: WireStreamFunction,
}

#[derive(Debug, Default, Deserialize)]
struct WireStreamFunction {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

#[derive(Debug, Serialize)]
struct WireMessage {
    role: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<WireToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
}

impl WireMessage {
    fn simple(role: &'static str, content: String) -> Self {
        Self {
            role,
            content: Some(content),
            tool_calls: None,
            tool_call_id: None,
        }
    }
}

#[derive(Debug, Serialize)]
struct WireToolCall {
    id: String,
    #[serde(rename = "type")]
    kind: &'static str,
    function: WireFunctionCall,
}

#[derive(Debug, Serialize)]
struct WireFunctionCall {
    name: String,
    arguments: String,
}

#[derive(Debug, Serialize)]
struct WireTool {
    #[serde(rename = "type")]
    kind: &'static str,
    function: WireToolFunction,
}

#[derive(Debug, Serialize)]
struct WireToolFunction {
    name: String,
    description: String,
    parameters: Value,
}

impl WireTool {
    fn from_definition(definition: &agent_domain::model::tool::ToolDefinition) -> Self {
        Self {
            kind: "function",
            function: WireToolFunction {
                name: definition.name.to_string(),
                description: definition.description.clone(),
                parameters: definition.input_schema.clone(),
            },
        }
    }
}

#[derive(Debug, Deserialize)]
struct WireResponse {
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    choices: Vec<WireChoice>,
    #[serde(default)]
    usage: Option<WireUsage>,
}

#[derive(Debug, Deserialize)]
struct WireChoice {
    message: WireResponseMessage,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct WireResponseMessage {
    #[serde(default)]
    content: Option<Value>,
    #[serde(default)]
    tool_calls: Vec<WireResponseToolCall>,
}

#[derive(Debug, Deserialize)]
struct WireResponseToolCall {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    function: WireResponseFunction,
}

#[derive(Debug, Default, Deserialize)]
struct WireResponseFunction {
    #[serde(default)]
    name: String,
    #[serde(default)]
    arguments: Value,
}

#[derive(Debug, Deserialize)]
struct WireUsage {
    #[serde(default)]
    prompt_tokens: Option<u64>,
    #[serde(default)]
    completion_tokens: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_domain::model::tool::ToolResult;
    use serde_json::json;

    fn provider() -> OpenAiCompatibleProvider {
        OpenAiCompatibleProvider::new(
            ProviderId::new("test"),
            "http://localhost:11434/v1",
            None,
            ModelId::new("qwen3:8b"),
            "max_tokens",
            Duration::from_secs(30),
        )
        .unwrap()
    }

    #[test]
    fn arguments_are_accepted_as_string_or_object() {
        assert_eq!(
            normalize_arguments(json!(r#"{"path":"a.rs"}"#)).unwrap(),
            json!({"path":"a.rs"})
        );
        assert_eq!(
            normalize_arguments(json!({"path":"a.rs"})).unwrap(),
            json!({"path":"a.rs"})
        );
        assert_eq!(normalize_arguments(json!("")).unwrap(), json!({}));
        assert_eq!(normalize_arguments(Value::Null).unwrap(), json!({}));
        assert!(normalize_arguments(json!("not json")).is_err());
    }

    #[test]
    fn tool_results_become_individual_tool_role_messages() {
        let call = ToolCall::new(
            ToolCallId::new("call_1"),
            ToolName::new("read_file").unwrap(),
            json!({}),
        );
        let message = Message::tool_results(vec![
            ToolResult::ok(&call, "first"),
            ToolResult::ok(&call, "second"),
        ]);

        let mut wire = Vec::new();
        append_message(&mut wire, &message);

        assert_eq!(wire.len(), 2);
        assert_eq!(wire[0].role, "tool");
        assert_eq!(wire[0].tool_call_id.as_deref(), Some("call_1"));
        assert_eq!(wire[1].content.as_deref(), Some("second"));
    }

    #[test]
    fn assistant_tool_calls_are_serialised_as_argument_strings() {
        let call = ToolCall::new(
            ToolCallId::new("call_1"),
            ToolName::new("read_file").unwrap(),
            json!({"path": "a.rs"}),
        );
        let message = Message::assistant(vec![ContentBlock::ToolCall(call)]);

        let mut wire = Vec::new();
        append_message(&mut wire, &message);

        let serialised = serde_json::to_value(&wire[0]).unwrap();
        assert_eq!(
            serialised["tool_calls"][0]["function"]["arguments"],
            json!(r#"{"path":"a.rs"}"#)
        );
    }

    #[test]
    fn max_tokens_field_name_is_configurable() {
        let body = WireRequest {
            model: "m",
            messages: vec![],
            tools: vec![],
            temperature: None,
            top_p: None,
            stop: vec![],
            max_tokens: BTreeMap::from([("max_completion_tokens".to_string(), 128)]),
            stream: false,
            stream_options: None,
        };
        let serialised = serde_json::to_value(&body).unwrap();
        assert_eq!(serialised["max_completion_tokens"], json!(128));
        assert!(serialised.get("max_tokens").is_none());
    }

    #[test]
    fn tool_calls_win_over_a_stop_finish_reason() {
        // Ollama reports finish_reason="stop" even when it emitted tool calls.
        let response = WireResponse {
            model: Some("qwen3:8b".into()),
            choices: vec![WireChoice {
                message: WireResponseMessage {
                    content: Some(json!("")),
                    tool_calls: vec![WireResponseToolCall {
                        id: None,
                        function: WireResponseFunction {
                            name: "read_file".into(),
                            arguments: json!({"path": "a.rs"}),
                        },
                    }],
                },
                finish_reason: Some("stop".into()),
            }],
            usage: None,
        };

        let decoded = provider()
            .decode(response, ModelId::new("qwen3:8b"))
            .unwrap();
        assert_eq!(decoded.stop_reason, StopReason::ToolUse);
        let call = decoded.message.tool_calls().next().unwrap();
        assert_eq!(call.name.as_str(), "read_file");
        assert_eq!(
            call.id.as_str(),
            "call_0",
            "a missing id gets a positional fallback"
        );
    }

    #[test]
    fn decodes_a_plain_text_answer() {
        let response = WireResponse {
            model: None,
            choices: vec![WireChoice {
                message: WireResponseMessage {
                    content: Some(json!("hello")),
                    tool_calls: vec![],
                },
                finish_reason: Some("stop".into()),
            }],
            usage: Some(WireUsage {
                prompt_tokens: Some(10),
                completion_tokens: Some(3),
            }),
        };

        let decoded = provider()
            .decode(response, ModelId::new("qwen3:8b"))
            .unwrap();
        assert_eq!(decoded.message.text(), "hello");
        assert_eq!(decoded.stop_reason, StopReason::EndTurn);
        assert_eq!(decoded.usage.total(), 13);
        assert_eq!(
            decoded.model.as_str(),
            "qwen3:8b",
            "falls back to the requested model"
        );
    }

    #[test]
    fn decodes_array_shaped_content() {
        assert_eq!(
            content_to_text(Some(json!([{"type": "text", "text": "a"}, {"text": "b"}]))),
            Some("ab".to_string())
        );
    }

    #[test]
    fn empty_choices_are_an_error() {
        let response = WireResponse {
            model: None,
            choices: vec![],
            usage: None,
        };
        assert!(provider().decode(response, ModelId::new("m")).is_err());
    }
}
