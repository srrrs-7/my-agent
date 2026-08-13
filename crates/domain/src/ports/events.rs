//! Observability port.
//!
//! The agent loop never prints. It emits events, and whatever drives it - a
//! TTY renderer, a JSON log, a future web UI - decides how to present them.

use std::time::Duration;

use crate::model::llm::{ModelId, ProviderId, StopReason, TokenUsage};
use crate::model::tool::{ToolCall, ToolName, ToolSafety};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FinishReason {
    /// The model produced a final answer.
    Completed,
    /// The iteration budget ran out before the model settled.
    MaxIterations { limit: u32 },
    /// The provider stopped for its own reason (token limit, stop sequence).
    Stopped { stop_reason: String },
}

#[derive(Debug, Clone, PartialEq)]
pub enum AgentEvent {
    RunStarted {
        session_id: String,
        provider: ProviderId,
        model: Option<ModelId>,
    },
    IterationStarted {
        iteration: u32,
        limit: u32,
    },
    HistoryTrimmed {
        dropped_messages: usize,
    },
    ModelResponded {
        provider: ProviderId,
        model: ModelId,
        stop_reason: StopReason,
        usage: TokenUsage,
        latency: Duration,
    },
    AssistantMessage {
        text: String,
    },
    ToolCallStarted {
        call: ToolCall,
        safety: ToolSafety,
    },
    ToolCallFinished {
        name: ToolName,
        call_id: String,
        is_error: bool,
        summary: String,
        duration: Duration,
    },
    ToolCallDenied {
        name: ToolName,
        reason: String,
    },
    RunFinished {
        reason: FinishReason,
        iterations: u32,
        usage: TokenUsage,
    },
}

/// Implementations must be cheap and non-blocking: the loop calls this on its
/// hot path and does not await it.
pub trait EventSink: Send + Sync {
    fn emit(&self, event: AgentEvent);
}

/// Discards everything. Useful in tests and for non-interactive embedding.
#[derive(Debug, Clone, Copy, Default)]
pub struct NullEventSink;

impl EventSink for NullEventSink {
    fn emit(&self, _event: AgentEvent) {}
}
