//! Observability port.
//!
//! The agent loop never prints. It emits events, and whatever drives it - a
//! TTY renderer, a JSON log, a future web UI - decides how to present them.

use std::time::Duration;

use crate::model::dependency::DependencyFileChange;
use crate::model::llm::{ModelId, ProviderId, StopReason, TokenUsage};
use crate::model::plan::Plan;
use crate::model::tool::{ToolCall, ToolCallId, ToolName, ToolSafety};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FinishReason {
    /// The model produced a final answer.
    Completed,
    /// The iteration budget ran out before the model settled.
    MaxIterations { limit: u32 },
    /// The provider stopped for its own reason (token limit, stop sequence).
    Stopped { stop_reason: String },
    /// Someone asked the turn to stop (Ctrl-C). The history is left well
    /// formed - every `tool_call` answered - so the session can carry on.
    Interrupted,
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
    /// Old turns were dropped outright. What they contained is gone.
    HistoryTrimmed {
        dropped_messages: usize,
    },
    /// Old turns were folded into a summary instead of being dropped. Distinct
    /// from [`Self::HistoryTrimmed`] because the two mean opposite things to
    /// someone reading the output: one says context was lost, the other says it
    /// was preserved in a smaller form - at the cost of one extra request.
    HistoryCompacted {
        folded_messages: usize,
        before_bytes: usize,
        after_bytes: usize,
    },
    ModelResponded {
        provider: ProviderId,
        model: ModelId,
        stop_reason: StopReason,
        usage: TokenUsage,
        latency: Duration,
    },
    /// A streamed fragment of the assistant's prose, for incremental display.
    /// The full text still arrives as [`AgentEvent::AssistantMessage`] once the
    /// turn completes; sinks that render deltas must treat that message as the
    /// terminator of the streamed text, not as a second copy.
    AssistantDelta {
        text: String,
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
        call_id: ToolCallId,
        is_error: bool,
        summary: String,
        duration: Duration,
    },
    ToolCallDenied {
        name: ToolName,
        reason: String,
    },
    /// A tool call changed dependency files without going through the file
    /// tools - a command such as `cargo add` or `npm install`. Found by
    /// comparing the workspace before and after, so it is a report, not a
    /// question: the change has already happened.
    DependenciesChanged {
        name: ToolName,
        files: Vec<DependencyFileChange>,
    },
    /// The comparison behind [`Self::DependenciesChanged`] did not cover the
    /// whole workspace, so a change may have gone unreported. Separate from
    /// it because "nothing changed" and "not everything was checked" must not
    /// look alike on the terminal.
    DependencyCheckIncomplete {
        name: ToolName,
        /// The scan's entry limit, the usual reason it stopped.
        limit: usize,
    },
    /// The model's work plan changed. Emitted only when it differs from the
    /// one already shown, so a sink can redraw it unconditionally. An empty
    /// plan means the model cleared it.
    PlanUpdated {
        plan: Plan,
    },
    RunFinished {
        reason: FinishReason,
        iterations: u32,
        usage: TokenUsage,
    },
    /// Something a sub-agent did, started by the `spawn_agent` call
    /// `parent_call_id` of this run.
    ///
    /// Wrapped rather than emitted as it is, because the child's events mean
    /// something else to every reader: its answer is not this run's answer
    /// (it reaches the model as the call's result, and must never reach
    /// stdout), its iterations are not this run's, and its tool calls are not
    /// calls this run's model made. A sink that does not care about
    /// sub-agents ignores the variant and stays correct.
    Nested {
        parent_call_id: ToolCallId,
        event: Box<AgentEvent>,
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
