//! Thinking, effort, and the content blocks sent to and read from the API.
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

use agent_domain::error::LlmError;
use agent_domain::model::message::{ContentBlock, Message};
use agent_domain::model::tool::{ToolCall, ToolCallId, ToolName};
use serde::{Deserialize, Serialize};
use serde_json::Value;

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

/// The [`ContentBlock::Opaque`] dialect this adapter writes and reads back.
pub(super) const DIALECT: &str = "anthropic";

/// The response's content blocks in the domain's terms.
pub(super) fn from_blocks(blocks: Vec<Value>) -> Result<Vec<ContentBlock>, LlmError> {
    let mut content = Vec::new();
    for raw in blocks {
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
    Ok(content)
}

pub(super) fn to_blocks(message: &Message) -> Vec<WireBlock> {
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
            ContentBlock::Image(image) => Some(WireBlock::Image {
                source: WireImageSource {
                    kind: "base64",
                    media_type: image.media_type().as_mime(),
                    data: image.to_base64(),
                },
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

#[derive(Debug, Serialize)]
pub(super) struct WireThinking {
    #[serde(rename = "type")]
    pub(super) kind: &'static str,
}

#[derive(Debug, Serialize)]
pub(super) struct WireOutputConfig {
    pub(super) effort: &'static str,
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(super) enum WireBlock {
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
    Image {
        source: WireImageSource,
    },
    /// A block received from the API and returned as it came (thinking).
    /// It carries its own `type`.
    #[serde(untagged)]
    Verbatim(Value),
}

#[derive(Debug, Serialize)]
pub(super) struct WireImageSource {
    #[serde(rename = "type")]
    kind: &'static str,
    media_type: &'static str,
    data: String,
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
