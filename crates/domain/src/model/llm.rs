//! Provider-agnostic request/response vocabulary.
//!
//! Everything a future routing layer needs to make a decision is expressed
//! here - notably [`RequestMetadata`], which travels with the request so a
//! router can pick a model from *intent* rather than from string matching.

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};

use super::message::Message;
use super::tool::ToolDefinition;

/// Stable identifier of a configured provider instance (`"local"`, `"cloud"`).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ProviderId(String);

impl ProviderId {
    pub fn new(raw: impl Into<String>) -> Self {
        Self(raw.into().trim().to_ascii_lowercase())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ProviderId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The model name as the *provider* understands it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ModelId(String);

impl ModelId {
    pub fn new(raw: impl Into<String>) -> Self {
        Self(raw.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Splits a `"<provider>/<model>"` reference. Used by the model-prefix
    /// router; returns `None` when the id carries no provider hint.
    ///
    /// Only the *first* `/` is treated as a separator, so vendor ids that
    /// legitimately contain slashes (`meta-llama/Llama-3.1-8B`) still work
    /// once the alias is known not to match.
    pub fn split_provider_hint(&self) -> Option<(&str, &str)> {
        self.0.split_once('/')
    }
}

impl fmt::Display for ModelId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct GenerationParams {
    pub temperature: Option<f32>,
    pub max_tokens: Option<u32>,
    pub top_p: Option<f32>,
    #[serde(default)]
    pub stop_sequences: Vec<String>,
}

/// Coarse intent of a call. A router can map this onto a cheap or an expensive
/// model without inspecting the prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum TaskKind {
    /// The main agent loop: reasoning plus tool use.
    #[default]
    Agentic,
    /// Plain conversational answer, no tools.
    Chat,
    /// History compaction / summarisation.
    Summarize,
    /// Short structured classification (routing, intent detection).
    Classify,
}

/// Out-of-band information about a request. Ignored by simple providers,
/// consumed by [`crate::ports::llm::LlmRouter`].
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct RequestMetadata {
    pub session_id: String,
    pub iteration: u32,
    pub task_kind: TaskKind,
    pub requires_tools: bool,
    /// Free-form hints (`"latency" => "low"`, `"tenant" => "acme"`).
    #[serde(default)]
    pub hints: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatRequest {
    /// `None` means "provider default".
    pub model: Option<ModelId>,
    pub system: Option<String>,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolDefinition>,
    pub params: GenerationParams,
    pub metadata: RequestMetadata,
}

impl ChatRequest {
    pub fn new(messages: Vec<Message>) -> Self {
        Self {
            model: None,
            system: None,
            messages,
            tools: Vec::new(),
            params: GenerationParams::default(),
            metadata: RequestMetadata::default(),
        }
    }

    pub fn with_system(mut self, system: impl Into<String>) -> Self {
        self.system = Some(system.into());
        self
    }

    pub fn with_tools(mut self, tools: Vec<ToolDefinition>) -> Self {
        self.metadata.requires_tools = !tools.is_empty();
        self.tools = tools;
        self
    }

    pub fn with_model(mut self, model: Option<ModelId>) -> Self {
        self.model = model;
        self
    }

    pub fn with_params(mut self, params: GenerationParams) -> Self {
        self.params = params;
        self
    }

    pub fn with_metadata(mut self, metadata: RequestMetadata) -> Self {
        self.metadata = metadata;
        self
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct TokenUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

impl TokenUsage {
    pub fn total(&self) -> u64 {
        self.input_tokens + self.output_tokens
    }

    pub fn accumulate(&mut self, other: TokenUsage) {
        self.input_tokens += other.input_tokens;
        self.output_tokens += other.output_tokens;
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    /// The model finished its turn and expects the user to speak next.
    EndTurn,
    /// The model asked for one or more tools.
    ToolUse,
    MaxTokens,
    StopSequence,
    Other(String),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatResponse {
    pub message: Message,
    pub stop_reason: StopReason,
    pub usage: TokenUsage,
    /// The model that actually served the request - may differ from the
    /// requested one once routing is in play.
    pub model: ModelId,
    pub provider: ProviderId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderCapabilities {
    pub supports_tools: bool,
    pub supports_streaming: bool,
    pub supports_system_prompt: bool,
    pub max_context_tokens: Option<u32>,
}

impl Default for ProviderCapabilities {
    fn default() -> Self {
        Self {
            supports_tools: true,
            supports_streaming: false,
            supports_system_prompt: true,
            max_context_tokens: None,
        }
    }
}

impl ProviderCapabilities {
    /// Conservative merge, used by the routing provider: it can only promise
    /// what *every* candidate behind it can deliver.
    pub fn intersect(self, other: Self) -> Self {
        Self {
            supports_tools: self.supports_tools && other.supports_tools,
            supports_streaming: self.supports_streaming && other.supports_streaming,
            supports_system_prompt: self.supports_system_prompt && other.supports_system_prompt,
            max_context_tokens: match (self.max_context_tokens, other.max_context_tokens) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (a, b) => a.or(b),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_id_splits_provider_hint() {
        let id = ModelId::new("cloud/claude-sonnet-5");
        assert_eq!(id.split_provider_hint(), Some(("cloud", "claude-sonnet-5")));
        assert_eq!(ModelId::new("qwen3:8b").split_provider_hint(), None);
    }

    #[test]
    fn capabilities_intersect_conservatively() {
        let a = ProviderCapabilities {
            supports_tools: true,
            supports_streaming: true,
            supports_system_prompt: true,
            max_context_tokens: Some(200_000),
        };
        let b = ProviderCapabilities {
            supports_tools: false,
            supports_streaming: true,
            supports_system_prompt: true,
            max_context_tokens: Some(32_000),
        };
        let merged = a.intersect(b);
        assert!(!merged.supports_tools);
        assert_eq!(merged.max_context_tokens, Some(32_000));
    }
}
