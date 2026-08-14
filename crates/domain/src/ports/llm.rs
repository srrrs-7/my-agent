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
//! OpenAiCompatible     RoutingProvider ---- dyn LlmRouter
//!                            |
//!                    +-------+-------+
//!                    |               |
//!              "local" provider  "cloud" provider
//! ```

use std::pin::Pin;
use std::task::{Context, Poll};

use async_trait::async_trait;
use futures_core::Stream;

use crate::error::LlmError;
use crate::model::llm::{ChatRequest, ChatResponse, ModelId, ProviderCapabilities, ProviderId};

/// One increment of a streamed chat completion.
///
/// The protocol is deliberately minimal so that invariant "every `tool_call`
/// is dispatched with complete arguments" holds by construction:
///
/// * [`StreamEvent::TextDelta`] carries assistant prose for *display only*.
///   For a streaming provider, the concatenation of all deltas equals the text
///   of the final message.
/// * [`StreamEvent::Completed`] is always the last successful event and holds
///   the same fully-assembled [`ChatResponse`] a non-streaming call would have
///   returned - tool calls included, with their arguments aggregated.
///   Consumers act on this event alone; deltas never carry tool calls.
///
/// A provider that cannot stream yields only `Completed` (see the default
/// [`LlmProvider::chat_stream`]); consumers must treat "no deltas" as a
/// non-streamed answer, not as an empty one.
#[derive(Debug, Clone, PartialEq)]
pub enum StreamEvent {
    TextDelta(String),
    Completed(ChatResponse),
}

/// A streamed chat completion. `Send` so it can cross task boundaries;
/// `'static` so a provider does not lend out its own internals.
pub type ChatStream = Pin<Box<dyn Stream<Item = Result<StreamEvent, LlmError>> + Send + 'static>>;

/// Access to a chat-completion model.
#[async_trait]
pub trait LlmProvider: Send + Sync {
    /// Stable identity used in logs, events and routing tables.
    fn id(&self) -> ProviderId;

    /// What this provider can do. The loop consults it before advertising
    /// tools, so a tool-less endpoint degrades gracefully instead of erroring.
    fn capabilities(&self) -> ProviderCapabilities;

    async fn chat(&self, request: ChatRequest) -> Result<ChatResponse, LlmError>;

    /// Streaming variant of [`Self::chat`].
    ///
    /// The default implementation falls back to the non-streaming call and
    /// yields a single [`StreamEvent::Completed`], so every provider (and any
    /// decorator that merely forwards) supports this method without knowing
    /// about streaming. An error from this method means the stream never
    /// started - retrying is safe. Errors *inside* the stream mean it broke
    /// mid-flight; resuming is the consumer's problem and is out of scope.
    async fn chat_stream(&self, request: ChatRequest) -> Result<ChatStream, LlmError> {
        let response = self.chat(request).await?;
        Ok(Box::pin(Once(Some(Ok(StreamEvent::Completed(response))))))
    }
}

/// Single-item stream used by the `chat_stream` fallback. Hand-rolled because
/// this crate deliberately depends on `futures-core` (the `Stream` trait) but
/// not on the combinator crates.
struct Once(Option<Result<StreamEvent, LlmError>>);

impl Stream for Once {
    type Item = Result<StreamEvent, LlmError>;

    fn poll_next(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Poll::Ready(self.get_mut().0.take())
    }
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
