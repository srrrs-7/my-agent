//! The LLM port and the routing seam.
//!
//! [`LlmProvider`] is intentionally the *only* thing the agent loop knows about
//! model access. Because a router is itself a provider (composite pattern), the
//! loop is unaffected when single-endpoint access is later replaced by
//! cost/latency/capability-aware routing:
//!
//! ```text
//!            AgentLoop
//!                |
//!         dyn LlmProvider
//!                |
//!     +----------+-----------+
//!     |                      |
//! OpenAiCompatible   RoutingLlmProvider ---- dyn LlmRouter
//!                            |
//!                    +-------+-------+
//!                    |               |
//!              "local" provider  "cloud" provider
//! ```

use async_trait::async_trait;

use crate::error::LlmError;
use crate::model::llm::{ChatRequest, ChatResponse, ModelId, ProviderCapabilities, ProviderId};

/// Access to a chat-completion model.
#[async_trait]
pub trait LlmProvider: Send + Sync {
    /// Stable identity used in logs, events and routing tables.
    fn id(&self) -> ProviderId;

    /// What this provider can do. The loop consults it before advertising
    /// tools, so a tool-less endpoint degrades gracefully instead of erroring.
    fn capabilities(&self) -> ProviderCapabilities;

    async fn chat(&self, request: ChatRequest) -> Result<ChatResponse, LlmError>;
}

/// Which provider (and optionally which model) should serve a request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteDecision {
    pub provider: ProviderId,
    /// `None` keeps whatever the request already asked for.
    pub model: Option<ModelId>,
    /// Human-readable justification, surfaced in traces.
    pub reason: String,
}

impl RouteDecision {
    pub fn to(provider: ProviderId, reason: impl Into<String>) -> Self {
        Self {
            provider,
            model: None,
            reason: reason.into(),
        }
    }

    pub fn with_model(mut self, model: ModelId) -> Self {
        self.model = Some(model);
        self
    }
}

/// Chooses a provider for a request.
///
/// Kept separate from [`LlmProvider`] so that policies (static, model-prefix,
/// cost-aware, latency-aware, failover, A/B) can be developed and tested
/// without touching transport code.
#[async_trait]
pub trait LlmRouter: Send + Sync {
    fn name(&self) -> &str;

    async fn route(&self, request: &ChatRequest) -> Result<RouteDecision, LlmError>;
}
