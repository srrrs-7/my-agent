//! Counting what the loop did, from the events it already emits.
//!
//! An [`EventSink`] rather than a hook in the loop: the loop narrates
//! everything worth measuring anyway, and a second channel for the same facts
//! would be one more thing that could disagree with the first.

use std::sync::Mutex;
use std::time::Duration;

use agent_domain::ports::events::{AgentEvent, EventSink};

/// What one run of the loop amounted to, beyond its outcome.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RunMetrics {
    /// Iterations started. Also known when the run ended in an error, which
    /// the outcome's own count is not.
    pub iterations: u32,
    /// Model round-trips of the loop itself. Summary requests are not among
    /// them: they emit no response event (their tokens are in the session
    /// usage instead).
    pub model_requests: u32,
    /// Time spent waiting on the model, summed over `model_requests`.
    pub model_latency: Duration,
    pub compactions: u32,
    pub trims: u32,
    pub tool_calls: u32,
    /// Calls that ran and came back flagged as errors.
    pub tool_errors: u32,
    /// Calls the approval gate refused, which never ran at all.
    pub denials: u32,
}

/// Accumulates [`RunMetrics`] from the events of one run.
///
/// One sink per run: the counters are never reset.
#[derive(Debug, Default)]
pub struct MetricsSink {
    metrics: Mutex<RunMetrics>,
}

impl MetricsSink {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn snapshot(&self) -> RunMetrics {
        *self.lock()
    }

    /// A panic elsewhere while holding the lock leaves plain counters behind,
    /// which are still worth reading - so a poisoned lock is not an error.
    fn lock(&self) -> std::sync::MutexGuard<'_, RunMetrics> {
        self.metrics
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl EventSink for MetricsSink {
    fn emit(&self, event: AgentEvent) {
        let mut metrics = self.lock();
        match event {
            AgentEvent::IterationStarted { .. } => metrics.iterations += 1,
            AgentEvent::ModelResponded { latency, .. } => {
                metrics.model_requests += 1;
                metrics.model_latency += latency;
            }
            AgentEvent::HistoryCompacted { .. } => metrics.compactions += 1,
            AgentEvent::HistoryTrimmed { .. } => metrics.trims += 1,
            AgentEvent::ToolCallStarted { .. } => metrics.tool_calls += 1,
            AgentEvent::ToolCallFinished { is_error, .. } => {
                if is_error {
                    metrics.tool_errors += 1;
                }
            }
            AgentEvent::ToolCallDenied { .. } => metrics.denials += 1,
            AgentEvent::RunStarted { .. }
            | AgentEvent::AssistantDelta { .. }
            | AgentEvent::AssistantMessage { .. }
            | AgentEvent::DependenciesChanged { .. }
            | AgentEvent::DependencyCheckIncomplete { .. }
            | AgentEvent::PlanUpdated { .. }
            | AgentEvent::RunFinished { .. } => {}
            // The run's metrics are the parent loop's: its `spawn_agent` call
            // is one tool call, and what the sub-agent spent is already in
            // the run's usage. Counting the child's iterations or calls here
            // would make a run that delegated look like one that looped.
            AgentEvent::Nested { .. } => {}
        }
    }
}
