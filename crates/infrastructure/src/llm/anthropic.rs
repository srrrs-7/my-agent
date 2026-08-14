//! Anthropic Messages API client (`POST /v1/messages`).
//!
//! Two shape differences from the OpenAI dialect are handled here:
//!
//! * the system prompt is a top-level field, not a message;
//! * tool results are `tool_result` blocks inside a **user** message, and
//!   consecutive messages of the same role are merged, because the API expects
//!   user and assistant turns to alternate.

use std::time::Duration;

use agent_domain::error::LlmError;
use agent_domain::model::llm::{
    ChatRequest, ChatResponse, ModelId, ProviderCapabilities, ProviderId, StopReason, TokenUsage,
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

pub struct AnthropicProvider {
    id: ProviderId,
    client: reqwest::Client,
    base_url: String,
    api_key: String,
    default_model: ModelId,
    timeout: Duration,
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
        })
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

        let (system, messages) = split_system(&request);

        let body = WireRequest {
            model: model.as_str(),
            max_tokens: request.params.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS),
            system,
            messages,
            tools: request
                .tools
                .iter()
                .map(WireTool::from_definition)
                .collect(),
            temperature: request.params.temperature,
            top_p: request.params.top_p,
            stop_sequences: request.params.stop_sequences.clone(),
        };

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
        for block in response.content {
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
            .map(|usage| TokenUsage {
                input_tokens: usage.input_tokens.unwrap_or_default(),
                output_tokens: usage.output_tokens.unwrap_or_default(),
            })
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

/// Splits the domain request into the API's `system` field plus an alternating
/// message list.
fn split_system(request: &ChatRequest) -> (Option<String>, Vec<WireMessage>) {
    let mut system_parts: Vec<String> = request.system.iter().cloned().collect();
    let mut messages: Vec<WireMessage> = Vec::with_capacity(request.messages.len());

    for message in &request.messages {
        if message.role == Role::System {
            // A system message inside the history is folded into the top-level
            // field rather than dropped.
            system_parts.push(message.text());
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

    let system = if system_parts.is_empty() {
        None
    } else {
        Some(system_parts.join("\n\n"))
    };
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
        })
        .collect()
}

// --- wire types --------------------------------------------------------------

#[derive(Debug, Serialize)]
struct WireRequest<'a> {
    model: &'a str,
    max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<String>,
    messages: Vec<WireMessage>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<WireTool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    top_p: Option<f32>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    stop_sequences: Vec<String>,
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
    #[serde(default)]
    content: Vec<WireResponseBlock>,
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
    #[serde(default)]
    input_tokens: Option<u64>,
    #[serde(default)]
    output_tokens: Option<u64>,
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

        let (system, messages) = split_system(&request);

        assert_eq!(system.as_deref(), Some("be helpful"));
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
                {"type": "thinking", "thinking": "..."},
                {"type": "text", "text": "done"}
            ],
            "stop_reason": "end_turn"
        }))
        .unwrap();

        let decoded = provider().decode(response, ModelId::new("m")).unwrap();
        assert_eq!(decoded.message.text(), "done");
    }
}
