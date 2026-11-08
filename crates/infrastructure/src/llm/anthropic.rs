//! Anthropic Messages API client (`POST /v1/messages`).
//!
//! Two shape differences from the OpenAI dialect are handled here:
//!
//! * the system prompt is a top-level field, not a message;
//! * tool results are `tool_result` blocks inside a **user** message, and
//!   consecutive messages of the same role are merged, because the API expects
//!   user and assistant turns to alternate.
//!
//! ## Prompt caching
//!
//! Every turn of the agent loop re-sends the tool definitions, the system
//! prompt and the whole history so far, and Anthropic caches only what it is
//! told to - without markers each turn pays full price for all of it again.
//! Two markers are sent, both governed by [`PromptCache`]:
//!
//! * one on the last block of the request's own system prompt. The API renders
//!   `tools`, then `system`, then `messages`, so this caches the tool
//!   definitions and the system prompt together - the part that stays fixed
//!   for a whole session - and gives it a read point that survives whatever
//!   later happens to the history, compaction and trimming included;
//! * a top-level `cache_control` (automatic caching): the server puts a
//!   breakpoint on the last block and moves it forward as the conversation
//!   grows, so each turn reads everything the previous turn sent.
//!
//! A hit needs the prefix to be byte-identical, so everything in front of a
//! breakpoint must serialise the same way every time. It does: tools arrive
//! sorted by name from the registry, `serde_json` objects (the input schemas)
//! serialise in a fixed key order, and the loop builds the system prompt once
//! per session rather than once per run.
//!
//! A prefix shorter than the model's minimum (512 to 4096 tokens, depending on
//! the model) is silently not cached - no error, just zeros in the cache
//! counts. Neither feature needs a beta header.
//!
//! ## Thinking and effort
//!
//! [`Thinking::Adaptive`] sends `"thinking": {"type": "adaptive"}` and an
//! [`Effort`] sends `"output_config": {"effort": ...}`. Both are left out
//! unless configured, so the default request is the one sent before they
//! existed. `Off` therefore means "say nothing", never `{"type": "disabled"}`:
//! some current models cannot turn thinking off and answer that with a 400.
//! `budget_tokens` is not sent at all - current models reject it.
//!
//! A response may then carry `thinking` blocks (whose text is empty when the
//! model does not display its reasoning) and `redacted_thinking` blocks. They
//! are kept as [`ContentBlock::Opaque`] and sent back exactly as received in
//! every later request of the conversation: the API verifies their signature,
//! the model needs them to continue its reasoning across a tool call, and they
//! are part of the cached prefix. Both settings are per process, so they
//! cannot change mid-conversation - which would invalidate the cache.

use std::str::FromStr;
use std::time::Duration;

use agent_domain::error::LlmError;
use agent_domain::model::llm::{
    ChatRequest, ChatResponse, ModelId, ProviderCapabilities, ProviderId, StopReason, TaskKind,
    TokenUsage,
};
use agent_domain::model::message::{ContentBlock, Message, Role};
use agent_domain::model::tool::{ToolCall, ToolCallId, ToolDefinition, ToolName};
use agent_domain::ports::llm::LlmProvider;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::http;

const API_VERSION: &str = "2023-06-01";
/// `max_tokens` is required by the API, so a request without one still needs a
/// number.
const DEFAULT_MAX_TOKENS: u32 = 4096;

/// How long a cached prompt prefix lives (`AGENT_PROMPT_CACHE`).
///
/// Writing to the cache costs more than plain input (1.25x for five minutes,
/// 2x for an hour) and reading from it far less, and every read restarts the
/// clock. Five minutes therefore suits an agent loop, whose turns follow one
/// another well inside that window; the hour pays off only when a session
/// regularly sits idle between prompts for longer. `Off` is for gateways that
/// reject `cache_control`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PromptCache {
    Off,
    #[default]
    FiveMinutes,
    OneHour,
}

impl PromptCache {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::FiveMinutes => "5m",
            Self::OneHour => "1h",
        }
    }

    /// The marker both breakpoints carry. They must agree: the API rejects a
    /// longer TTL behind a shorter one.
    fn marker(self) -> Option<CacheControl> {
        match self {
            Self::Off => None,
            // Five minutes is the API's default, so it is left unsaid.
            Self::FiveMinutes => Some(CacheControl {
                kind: "ephemeral",
                ttl: None,
            }),
            Self::OneHour => Some(CacheControl {
                kind: "ephemeral",
                ttl: Some("1h"),
            }),
        }
    }
}

impl FromStr for PromptCache {
    type Err = String;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "5m" | "5min" | "on" | "true" => Ok(Self::FiveMinutes),
            "1h" | "60m" | "1hour" => Ok(Self::OneHour),
            "off" | "false" | "none" | "0" => Ok(Self::Off),
            other => Err(format!(
                "unknown prompt cache setting `{other}` (expected 5m|1h|off)"
            )),
        }
    }
}

/// Whether to ask for extended thinking (`AGENT_THINKING`).
///
/// There is deliberately no "disabled": on a model that thinks by default
/// `Off` leaves that default alone, because sending `{"type": "disabled"}` is
/// a 400 on the models that cannot stop thinking.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Thinking {
    /// The `thinking` field is not sent.
    #[default]
    Off,
    /// The model decides when and how much to think.
    Adaptive,
}

impl Thinking {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Adaptive => "adaptive",
        }
    }
}

impl FromStr for Thinking {
    type Err = String;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "off" | "false" | "none" | "0" => Ok(Self::Off),
            "adaptive" | "on" | "true" => Ok(Self::Adaptive),
            other => Err(format!(
                "unknown thinking setting `{other}` (expected off|adaptive)"
            )),
        }
    }
}

/// How much the model should spend on a response (`AGENT_EFFORT`), sent as
/// `output_config.effort`. Unset means the field is left out and the model's
/// own default applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Effort {
    Low,
    Medium,
    High,
    XHigh,
    Max,
}

impl Effort {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::XHigh => "xhigh",
            Self::Max => "max",
        }
    }
}

impl FromStr for Effort {
    type Err = String;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "low" => Ok(Self::Low),
            "medium" => Ok(Self::Medium),
            "high" => Ok(Self::High),
            "xhigh" => Ok(Self::XHigh),
            "max" => Ok(Self::Max),
            other => Err(format!(
                "unknown effort `{other}` (expected low|medium|high|xhigh|max)"
            )),
        }
    }
}

/// The settings that shape every request body, apart from the request itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct RequestOptions {
    prompt_cache: PromptCache,
    thinking: Thinking,
    effort: Option<Effort>,
}

/// The [`ContentBlock::Opaque`] dialect this adapter writes and reads back.
const DIALECT: &str = "anthropic";

pub struct AnthropicProvider {
    id: ProviderId,
    client: reqwest::Client,
    base_url: String,
    api_key: String,
    default_model: ModelId,
    timeout: Duration,
    options: RequestOptions,
}

impl AnthropicProvider {
    pub fn new(
        id: ProviderId,
        base_url: impl Into<String>,
        api_key: String,
        default_model: ModelId,
        timeout: Duration,
    ) -> Result<Self, LlmError> {
        Ok(Self {
            id,
            client: http::build_client(timeout)?,
            base_url: base_url.into().trim_end_matches('/').to_string(),
            api_key,
            default_model,
            timeout,
            options: RequestOptions::default(),
        })
    }

    #[must_use]
    pub fn with_prompt_cache(mut self, prompt_cache: PromptCache) -> Self {
        self.options.prompt_cache = prompt_cache;
        self
    }

    #[must_use]
    pub fn with_thinking(mut self, thinking: Thinking) -> Self {
        self.options.thinking = thinking;
        self
    }

    #[must_use]
    pub fn with_effort(mut self, effort: Option<Effort>) -> Self {
        self.options.effort = effort;
        self
    }

    fn messages_url(&self) -> String {
        // Tolerate a base url that already carries the version segment.
        if self.base_url.ends_with("/v1") {
            format!("{}/messages", self.base_url)
        } else {
            format!("{}/v1/messages", self.base_url)
        }
    }
}

#[async_trait]
impl LlmProvider for AnthropicProvider {
    fn id(&self) -> ProviderId {
        self.id.clone()
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            supports_tools: true,
            supports_streaming: false,
            supports_system_prompt: true,
            max_context_tokens: None,
        }
    }

    async fn chat(&self, request: ChatRequest) -> Result<ChatResponse, LlmError> {
        let model = request
            .model
            .clone()
            .unwrap_or_else(|| self.default_model.clone());

        let body = wire_request(&request, &model, self.options);

        let builder = self
            .client
            .post(self.messages_url())
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", API_VERSION)
            .json(&body);

        let text = http::send(builder, &self.base_url, self.timeout).await?;

        let parsed: WireResponse = serde_json::from_str(&text)
            .map_err(|error| LlmError::InvalidResponse(format!("{error}")))?;

        self.decode(parsed, model)
    }
}

impl AnthropicProvider {
    fn decode(
        &self,
        response: WireResponse,
        requested_model: ModelId,
    ) -> Result<ChatResponse, LlmError> {
        let mut content = Vec::new();
        for raw in response.content {
            // Kept as the JSON that arrived rather than parsed into fields:
            // the signature covers the exact block, and a field this adapter
            // does not know about yet must survive the round trip as well.
            if matches!(
                raw.get("type").and_then(Value::as_str),
                Some("thinking" | "redacted_thinking")
            ) {
                content.push(ContentBlock::Opaque {
                    dialect: DIALECT.to_string(),
                    data: raw,
                });
                continue;
            }
            let block: WireResponseBlock = serde_json::from_value(raw)
                .map_err(|error| LlmError::InvalidResponse(format!("content block: {error}")))?;
            match block {
                WireResponseBlock::Text { text } if !text.trim().is_empty() => {
                    content.push(ContentBlock::text(text));
                }
                WireResponseBlock::Text { .. } => {}
                WireResponseBlock::ToolUse { id, name, input } => {
                    let name = ToolName::new(name.clone()).map_err(|error| {
                        LlmError::InvalidResponse(format!(
                            "the model asked for an unusable tool name `{name}`: {error}"
                        ))
                    })?;
                    content.push(ContentBlock::ToolCall(ToolCall::new(
                        ToolCallId::new(id),
                        name,
                        input,
                    )));
                }
                WireResponseBlock::Unknown => {}
            }
        }

        let stop_reason = match response.stop_reason.as_deref() {
            Some("end_turn") | None => StopReason::EndTurn,
            Some("tool_use") => StopReason::ToolUse,
            Some("max_tokens") => StopReason::MaxTokens,
            Some("stop_sequence") => StopReason::StopSequence,
            Some(other) => StopReason::Other(other.to_string()),
        };

        let usage = response
            .usage
            .map(WireUsage::into_domain)
            .unwrap_or_default();

        Ok(ChatResponse {
            message: Message::assistant(content),
            stop_reason,
            usage,
            model: response.model.map(ModelId::new).unwrap_or(requested_model),
            provider: self.id.clone(),
        })
    }
}

// --- outbound mapping --------------------------------------------------------

fn wire_request<'a>(
    request: &ChatRequest,
    model: &'a ModelId,
    options: RequestOptions,
) -> WireRequest<'a> {
    let marker = options.prompt_cache.marker();
    let (system, messages) = split_system(request, marker);
    let thinking = match options.thinking {
        Thinking::Off => None,
        Thinking::Adaptive => Some(WireThinking { kind: "adaptive" }),
    };
    // Thinking does not combine with a sampling temperature: models that
    // accept both reject a non-default value once thinking is on. The
    // setting is a non-thinking knob, so it is dropped rather than failing
    // every request of the session.
    let sampling = thinking.is_none();

    WireRequest {
        model: model.as_str(),
        max_tokens: request.params.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS),
        system,
        messages,
        tools: request
            .tools
            .iter()
            .map(WireTool::from_definition)
            .collect(),
        temperature: request.params.temperature.filter(|_| sampling),
        top_p: request.params.top_p.filter(|_| sampling),
        stop_sequences: request.params.stop_sequences.clone(),
        cache_control: marker.filter(|_| resends_its_tail(request.metadata.task_kind)),
        thinking,
        output_config: options.effort.map(|effort| WireOutputConfig {
            effort: effort.as_str(),
        }),
    }
}

/// Whether a later request will start with everything this one sends.
///
/// Automatic caching writes everything up to the last block that is not
/// cached yet, at a premium over plain input. Each turn of a conversation earns
/// that back on the next turn; a one-off side call never does. Compaction is the case that matters:
/// its transcript is summarised once and never sent again, so marking it would
/// only make the largest request of the session a quarter dearer. Its system
/// prompt is still marked - that part *is* sent again by the next compaction.
fn resends_its_tail(kind: TaskKind) -> bool {
    match kind {
        TaskKind::Agentic | TaskKind::Chat => true,
        TaskKind::Summarize | TaskKind::Classify => false,
    }
}

/// Splits the domain request into the API's `system` blocks plus an
/// alternating message list.
///
/// `marker` lands on the request's own system prompt rather than on whatever
/// block happens to come last: that prompt is the part fixed for the session,
/// and a breakpoint is only worth having on a prefix that repeats.
fn split_system(
    request: &ChatRequest,
    marker: Option<CacheControl>,
) -> (Vec<WireSystemBlock>, Vec<WireMessage>) {
    let mut system: Vec<WireSystemBlock> = request
        .system
        .as_deref()
        .and_then(WireSystemBlock::text)
        .into_iter()
        .collect();
    if let Some(last) = system.last_mut() {
        last.cache_control = marker;
    }

    let mut messages: Vec<WireMessage> = Vec::with_capacity(request.messages.len());

    for message in &request.messages {
        if message.role == Role::System {
            // A system message inside the history is folded into the top-level
            // field rather than dropped.
            system.extend(WireSystemBlock::text(&message.text()));
            continue;
        }

        let role = match message.role {
            Role::Assistant => "assistant",
            // Tool results are user-turn content for this API.
            _ => "user",
        };

        let blocks = to_blocks(message);
        if blocks.is_empty() {
            continue;
        }

        match messages.last_mut() {
            Some(previous) if previous.role == role => previous.content.extend(blocks),
            _ => messages.push(WireMessage {
                role,
                content: blocks,
            }),
        }
    }

    (system, messages)
}

fn to_blocks(message: &Message) -> Vec<WireBlock> {
    message
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } if text.trim().is_empty() => None,
            ContentBlock::Text { text } => Some(WireBlock::Text { text: text.clone() }),
            ContentBlock::ToolCall(call) => Some(WireBlock::ToolUse {
                id: call.id.to_string(),
                name: call.name.to_string(),
                input: call.arguments.clone(),
            }),
            ContentBlock::ToolResult(result) => Some(WireBlock::ToolResult {
                tool_use_id: result.call_id.to_string(),
                content: result.content.clone(),
                is_error: result.is_error,
            }),
            ContentBlock::Opaque { dialect, data } if dialect == DIALECT => {
                Some(WireBlock::Verbatim(data.clone()))
            }
            // Another vendor's private content means nothing to this API.
            ContentBlock::Opaque { .. } => None,
        })
        .collect()
}

// --- wire types --------------------------------------------------------------

#[derive(Debug, Serialize)]
struct WireRequest<'a> {
    model: &'a str,
    max_tokens: u32,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    system: Vec<WireSystemBlock>,
    messages: Vec<WireMessage>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<WireTool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    top_p: Option<f32>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    stop_sequences: Vec<String>,
    /// Automatic caching. Uses one of the four breakpoints a request may
    /// carry; the system marker uses another.
    #[serde(skip_serializing_if = "Option::is_none")]
    cache_control: Option<CacheControl>,
    #[serde(skip_serializing_if = "Option::is_none")]
    thinking: Option<WireThinking>,
    #[serde(skip_serializing_if = "Option::is_none")]
    output_config: Option<WireOutputConfig>,
}

#[derive(Debug, Serialize)]
struct WireThinking {
    #[serde(rename = "type")]
    kind: &'static str,
}

#[derive(Debug, Serialize)]
struct WireOutputConfig {
    effort: &'static str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
struct CacheControl {
    #[serde(rename = "type")]
    kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    ttl: Option<&'static str>,
}

#[derive(Debug, Serialize)]
struct WireSystemBlock {
    #[serde(rename = "type")]
    kind: &'static str,
    text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    cache_control: Option<CacheControl>,
}

impl WireSystemBlock {
    /// `None` for blank text: the API rejects an empty text block, and one
    /// carrying the cache marker would fail the whole request.
    fn text(text: &str) -> Option<Self> {
        (!text.trim().is_empty()).then(|| Self {
            kind: "text",
            text: text.to_string(),
            cache_control: None,
        })
    }
}

#[derive(Debug, Serialize)]
struct WireMessage {
    role: &'static str,
    content: Vec<WireBlock>,
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum WireBlock {
    Text {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
    ToolResult {
        tool_use_id: String,
        content: String,
        is_error: bool,
    },
    /// A block received from the API and returned as it came (thinking).
    /// It carries its own `type`.
    #[serde(untagged)]
    Verbatim(Value),
}

#[derive(Debug, Serialize)]
struct WireTool {
    name: String,
    description: String,
    input_schema: Value,
}

impl WireTool {
    fn from_definition(definition: &ToolDefinition) -> Self {
        Self {
            name: definition.name.to_string(),
            description: definition.description.clone(),
            input_schema: definition.input_schema.clone(),
        }
    }
}

#[derive(Debug, Deserialize)]
struct WireResponse {
    #[serde(default)]
    model: Option<String>,
    /// Raw, so that thinking blocks can be kept exactly as they arrived;
    /// everything else is parsed into [`WireResponseBlock`] one by one.
    #[serde(default)]
    content: Vec<Value>,
    #[serde(default)]
    stop_reason: Option<String>,
    #[serde(default)]
    usage: Option<WireUsage>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum WireResponseBlock {
    Text {
        #[serde(default)]
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        #[serde(default)]
        input: Value,
    },
    /// Forward compatibility: unknown block types are ignored rather than
    /// failing the whole response.
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Deserialize)]
struct WireUsage {
    /// Only the part of the prompt that was neither read from nor written to
    /// the cache.
    #[serde(default)]
    input_tokens: Option<u64>,
    #[serde(default)]
    output_tokens: Option<u64>,
    #[serde(default)]
    cache_creation_input_tokens: Option<u64>,
    #[serde(default)]
    cache_read_input_tokens: Option<u64>,
}

impl WireUsage {
    /// The domain counts the whole prompt as input, so the three parts the
    /// API reports separately are added back together here.
    fn into_domain(self) -> TokenUsage {
        let read = self.cache_read_input_tokens.unwrap_or_default();
        let write = self.cache_creation_input_tokens.unwrap_or_default();
        TokenUsage {
            input_tokens: self.input_tokens.unwrap_or_default() + read + write,
            output_tokens: self.output_tokens.unwrap_or_default(),
            cache_read_tokens: read,
            cache_write_tokens: write,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_domain::model::tool::ToolResult;
    use serde_json::json;

    fn provider() -> AnthropicProvider {
        AnthropicProvider::new(
            ProviderId::new("cloud"),
            "https://api.anthropic.com",
            "sk-ant-test".into(),
            ModelId::new("claude-sonnet-5"),
            Duration::from_secs(30),
        )
        .unwrap()
    }

    fn call() -> ToolCall {
        ToolCall::new(
            ToolCallId::new("toolu_1"),
            ToolName::new("read_file").unwrap(),
            json!({"path": "a.rs"}),
        )
    }

    #[test]
    fn builds_the_versioned_url_only_once() {
        assert_eq!(
            provider().messages_url(),
            "https://api.anthropic.com/v1/messages"
        );

        let already_versioned = AnthropicProvider::new(
            ProviderId::new("cloud"),
            "https://gateway.example.com/anthropic/v1",
            "k".into(),
            ModelId::new("m"),
            Duration::from_secs(1),
        )
        .unwrap();
        assert_eq!(
            already_versioned.messages_url(),
            "https://gateway.example.com/anthropic/v1/messages"
        );
    }

    #[test]
    fn tool_results_are_merged_into_the_user_turn() {
        let request = ChatRequest::new(vec![
            Message::user("read it"),
            Message::assistant(vec![ContentBlock::ToolCall(call())]),
            Message::tool_results(vec![ToolResult::ok(&call(), "contents")]),
            Message::user("thanks"),
        ])
        .with_system("be helpful");

        let (system, messages) = split_system(&request, None);

        assert_eq!(system.len(), 1);
        assert_eq!(system[0].text, "be helpful");
        // user / assistant / user - the tool result and the follow-up user
        // message collapse into one turn.
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0].role, "user");
        assert_eq!(messages[1].role, "assistant");
        assert_eq!(messages[2].role, "user");
        assert_eq!(messages[2].content.len(), 2);
        assert!(matches!(
            messages[2].content[0],
            WireBlock::ToolResult { .. }
        ));
    }

    #[test]
    fn blocks_serialise_with_the_expected_tags() {
        let serialised = serde_json::to_value(WireBlock::ToolResult {
            tool_use_id: "toolu_1".into(),
            content: "ok".into(),
            is_error: false,
        })
        .unwrap();
        assert_eq!(serialised["type"], json!("tool_result"));
        assert_eq!(serialised["tool_use_id"], json!("toolu_1"));
    }

    #[test]
    fn decodes_tool_use_responses() {
        let response: WireResponse = serde_json::from_value(json!({
            "id": "msg_1",
            "type": "message",
            "role": "assistant",
            "model": "claude-sonnet-5",
            "content": [
                {"type": "text", "text": "Let me look."},
                {"type": "tool_use", "id": "toolu_1", "name": "read_file", "input": {"path": "a.rs"}}
            ],
            "stop_reason": "tool_use",
            "usage": {"input_tokens": 42, "output_tokens": 7}
        }))
        .unwrap();

        let decoded = provider()
            .decode(response, ModelId::new("claude-sonnet-5"))
            .unwrap();
        assert_eq!(decoded.stop_reason, StopReason::ToolUse);
        assert_eq!(decoded.message.text(), "Let me look.");
        assert_eq!(decoded.usage.total(), 49);
        assert_eq!(
            decoded.message.tool_calls().next().unwrap().id.as_str(),
            "toolu_1"
        );
    }

    #[test]
    fn unknown_block_types_are_skipped() {
        let response: WireResponse = serde_json::from_value(json!({
            "content": [
                {"type": "something_new", "payload": "..."},
                {"type": "text", "text": "done"}
            ],
            "stop_reason": "end_turn"
        }))
        .unwrap();

        let decoded = provider().decode(response, ModelId::new("m")).unwrap();
        assert_eq!(decoded.message.text(), "done");
        assert_eq!(decoded.message.content.len(), 1);
    }

    // --- thinking and effort ---------------------------------------------------

    /// A minimal request whose whole body can be compared.
    fn plain() -> ChatRequest {
        let mut request = ChatRequest::new(vec![Message::user("hi")]);
        request.params.temperature = Some(0.5); // exact in f32, so the JSON compares equal
        request.params.max_tokens = Some(1000);
        request
    }

    fn options(thinking: Thinking, effort: Option<Effort>) -> RequestOptions {
        RequestOptions {
            prompt_cache: PromptCache::Off,
            thinking,
            effort,
        }
    }

    #[test]
    fn with_thinking_off_and_no_effort_the_request_is_unchanged() {
        let body = body_with(&plain(), options(Thinking::Off, None));

        assert_eq!(
            body,
            json!({
                "model": "m",
                "max_tokens": 1000,
                "messages": [{"role": "user", "content": [{"type": "text", "text": "hi"}]}],
                "temperature": 0.5
            }),
            "off sends no `thinking` field at all - never `disabled`"
        );
    }

    #[test]
    fn adaptive_thinking_is_requested_and_the_temperature_is_dropped() {
        let body = body_with(&plain(), options(Thinking::Adaptive, None));

        assert_eq!(
            body,
            json!({
                "model": "m",
                "max_tokens": 1000,
                "messages": [{"role": "user", "content": [{"type": "text", "text": "hi"}]}],
                "thinking": {"type": "adaptive"}
            })
        );
    }

    #[test]
    fn effort_is_sent_inside_output_config_with_or_without_thinking() {
        let alone = body_with(&plain(), options(Thinking::Off, Some(Effort::XHigh)));
        assert_eq!(alone["output_config"], json!({"effort": "xhigh"}));
        assert!(alone.get("effort").is_none(), "never top-level: {alone}");
        assert!(alone.get("thinking").is_none(), "{alone}");

        let both = body_with(&plain(), options(Thinking::Adaptive, Some(Effort::Low)));
        assert_eq!(
            both,
            json!({
                "model": "m",
                "max_tokens": 1000,
                "messages": [{"role": "user", "content": [{"type": "text", "text": "hi"}]}],
                "thinking": {"type": "adaptive"},
                "output_config": {"effort": "low"}
            })
        );
        assert!(!both.to_string().contains("budget_tokens"));
    }

    #[test]
    fn thinking_blocks_are_sent_back_exactly_as_they_arrived() {
        let thinking = json!({
            "type": "thinking",
            "thinking": "",
            "signature": "EqQBCkYIARgCKkA=",
            "a_field_added_later": {"kept": true}
        });
        let redacted = json!({"type": "redacted_thinking", "data": "EmwKAhgBEgy3va3pzix/LafPsn4a"});
        let response: WireResponse = serde_json::from_value(json!({
            "content": [
                thinking.clone(),
                redacted.clone(),
                {"type": "text", "text": "Let me look."},
                {"type": "tool_use", "id": "toolu_1", "name": "read_file", "input": {"path": "a.rs"}}
            ],
            "stop_reason": "tool_use"
        }))
        .unwrap();
        let decoded = provider().decode(response, ModelId::new("m")).unwrap();

        assert_eq!(
            decoded.message.text(),
            "Let me look.",
            "reasoning is not text"
        );
        let call = decoded.message.tool_calls().next().unwrap().clone();
        let next = ChatRequest::new(vec![
            Message::user("read it"),
            decoded.message,
            Message::tool_results(vec![ToolResult::ok(&call, "contents")]),
        ]);
        let body = body_with(&next, options(Thinking::Adaptive, None));

        assert_eq!(
            body["messages"][1],
            json!({
                "role": "assistant",
                "content": [
                    thinking,
                    redacted,
                    {"type": "text", "text": "Let me look."},
                    {"type": "tool_use", "id": "toolu_1", "name": "read_file", "input": {"path": "a.rs"}}
                ]
            }),
            "same blocks, same order, same bytes"
        );
    }

    #[test]
    fn another_vendors_opaque_content_is_left_out() {
        let request = ChatRequest::new(vec![
            Message::user("hi"),
            Message::assistant(vec![
                ContentBlock::Opaque {
                    dialect: "someone-else".into(),
                    data: json!({"type": "reasoning", "id": "rs_1"}),
                },
                ContentBlock::text("answer"),
            ]),
        ]);

        let body = body_with(&request, options(Thinking::Off, None));

        assert_eq!(
            body["messages"][1]["content"],
            json!([{"type": "text", "text": "answer"}])
        );
    }

    #[test]
    fn thinking_and_effort_settings_parse_and_name_the_alternatives() {
        assert_eq!(Thinking::default(), Thinking::Off);
        assert_eq!(
            " Adaptive ".parse::<Thinking>().unwrap(),
            Thinking::Adaptive
        );
        assert_eq!("off".parse::<Thinking>().unwrap(), Thinking::Off);
        let error = "disabled".parse::<Thinking>().unwrap_err();
        assert!(error.contains("off|adaptive"), "{error}");

        for effort in [
            Effort::Low,
            Effort::Medium,
            Effort::High,
            Effort::XHigh,
            Effort::Max,
        ] {
            assert_eq!(effort.as_str().parse::<Effort>().unwrap(), effort);
        }
        let error = "extreme".parse::<Effort>().unwrap_err();
        assert!(error.contains("low|medium|high|xhigh|max"), "{error}");
    }

    // --- prompt caching ------------------------------------------------------

    fn tool(name: &str) -> ToolDefinition {
        ToolDefinition {
            name: ToolName::new(name).unwrap(),
            description: format!("{name} does things."),
            input_schema: json!({"type": "object", "properties": {"path": {"type": "string"}}}),
            safety: agent_domain::model::tool::ToolSafety::ReadOnly,
        }
    }

    /// The JSON body an agent-loop turn would put on the wire.
    fn body(request: &ChatRequest, prompt_cache: PromptCache) -> Value {
        body_with(
            request,
            RequestOptions {
                prompt_cache,
                ..RequestOptions::default()
            },
        )
    }

    fn body_with(request: &ChatRequest, options: RequestOptions) -> Value {
        serde_json::to_value(wire_request(request, &ModelId::new("m"), options)).unwrap()
    }

    fn loop_turn() -> ChatRequest {
        ChatRequest::new(vec![Message::user("hi")])
            .with_system("be helpful")
            .with_tools(vec![tool("read_file"), tool("write_file")])
    }

    #[test]
    fn by_default_the_system_prompt_and_the_conversation_are_both_cached() {
        let body = body(&loop_turn(), PromptCache::default());

        assert_eq!(
            body["system"],
            json!([{
                "type": "text",
                "text": "be helpful",
                "cache_control": {"type": "ephemeral"}
            }]),
            "the breakpoint on the system prompt caches the tools in front of it too"
        );
        assert_eq!(
            body["cache_control"],
            json!({"type": "ephemeral"}),
            "automatic caching follows the growing conversation"
        );
    }

    #[test]
    fn a_one_hour_ttl_is_set_on_both_breakpoints() {
        // A longer TTL may not follow a shorter one, so they must agree.
        let body = body(&loop_turn(), PromptCache::OneHour);

        let one_hour = json!({"type": "ephemeral", "ttl": "1h"});
        assert_eq!(body["system"][0]["cache_control"], one_hour);
        assert_eq!(body["cache_control"], one_hour);
    }

    #[test]
    fn turning_the_cache_off_sends_no_marker_anywhere() {
        let body = body(&loop_turn(), PromptCache::Off);

        let sent = body.to_string();
        assert!(!sent.contains("cache_control"), "{sent}");
        assert_eq!(
            body["system"],
            json!([{"type": "text", "text": "be helpful"}])
        );
    }

    #[test]
    fn tools_are_sent_unmarked_in_the_order_they_were_given() {
        // Reordering them would move bytes in front of the system breakpoint.
        let body = body(&loop_turn(), PromptCache::default());

        let tools = body["tools"].as_array().unwrap();
        let names: Vec<&str> = tools
            .iter()
            .map(|tool| tool["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["read_file", "write_file"]);
        assert!(
            tools.iter().all(|tool| tool.get("cache_control").is_none()),
            "{tools:?}"
        );
    }

    #[test]
    fn a_compaction_request_does_not_pay_to_cache_its_one_off_transcript() {
        let request = ChatRequest::new(vec![Message::user("<transcript>")])
            .with_system("summarise this")
            .with_metadata(agent_domain::model::llm::RequestMetadata {
                task_kind: TaskKind::Summarize,
                ..Default::default()
            });

        let body = body(&request, PromptCache::default());

        assert!(body.get("cache_control").is_none(), "{body}");
        assert_eq!(
            body["system"][0]["cache_control"],
            json!({"type": "ephemeral"}),
            "the summariser's instructions repeat across compactions, so they stay marked"
        );
    }

    #[test]
    fn a_blank_system_prompt_is_left_out_rather_than_sent_as_an_empty_block() {
        let request = ChatRequest::new(vec![Message::user("hi")]).with_system("  \n");

        let body = body(&request, PromptCache::default());

        assert!(body.get("system").is_none(), "{body}");
        assert_eq!(
            body["cache_control"],
            json!({"type": "ephemeral"}),
            "the conversation is still cached"
        );
    }

    #[test]
    fn a_system_message_from_the_history_follows_the_marked_prompt() {
        let request = ChatRequest::new(vec![
            Message::user("hi"),
            Message::new(Role::System, vec![ContentBlock::text("mid-session note")]),
        ])
        .with_system("base");

        let body = body(&request, PromptCache::default());

        assert_eq!(
            body["system"],
            json!([
                {"type": "text", "text": "base", "cache_control": {"type": "ephemeral"}},
                {"type": "text", "text": "mid-session note"}
            ]),
            "the breakpoint stays on the part that repeats"
        );
    }

    #[test]
    fn cache_counts_are_added_back_into_the_input() {
        let response: WireResponse = serde_json::from_value(json!({
            "content": [{"type": "text", "text": "ok"}],
            "stop_reason": "end_turn",
            "usage": {
                "input_tokens": 50,
                "output_tokens": 7,
                "cache_creation_input_tokens": 1000,
                "cache_read_input_tokens": 3000,
                "cache_creation": {"ephemeral_5m_input_tokens": 1000, "ephemeral_1h_input_tokens": 0}
            }
        }))
        .unwrap();

        let decoded = provider().decode(response, ModelId::new("m")).unwrap();

        assert_eq!(
            decoded.usage,
            TokenUsage {
                input_tokens: 4050,
                output_tokens: 7,
                cache_read_tokens: 3000,
                cache_write_tokens: 1000,
            },
            "the API's input_tokens is only the uncached remainder"
        );
    }

    #[test]
    fn missing_or_null_cache_counts_read_as_zero() {
        for usage in [
            json!({"input_tokens": 42, "output_tokens": 7}),
            json!({
                "input_tokens": 42,
                "output_tokens": 7,
                "cache_creation_input_tokens": null,
                "cache_read_input_tokens": null
            }),
        ] {
            let response: WireResponse =
                serde_json::from_value(json!({"content": [], "usage": usage})).unwrap();

            let decoded = provider().decode(response, ModelId::new("m")).unwrap();

            assert_eq!(
                decoded.usage,
                TokenUsage {
                    input_tokens: 42,
                    output_tokens: 7,
                    ..TokenUsage::default()
                },
                "{usage}"
            );
        }
    }

    #[test]
    fn prompt_cache_settings_parse_leniently_and_name_the_alternatives() {
        assert_eq!(
            "5m".parse::<PromptCache>().unwrap(),
            PromptCache::FiveMinutes
        );
        assert_eq!(" 1H ".parse::<PromptCache>().unwrap(), PromptCache::OneHour);
        assert_eq!("off".parse::<PromptCache>().unwrap(), PromptCache::Off);
        assert_eq!(PromptCache::default(), PromptCache::FiveMinutes);

        let error = "forever".parse::<PromptCache>().unwrap_err();
        assert!(error.contains("5m|1h|off"), "{error}");

        for cache in [
            PromptCache::Off,
            PromptCache::FiveMinutes,
            PromptCache::OneHour,
        ] {
            assert_eq!(cache.as_str().parse::<PromptCache>().unwrap(), cache);
        }
    }
}
