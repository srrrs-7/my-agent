//! Evaluation: running a suite of tasks against a real model and measuring
//! how it did.
//!
//! The unit tests say whether the loop keeps its contracts. They cannot say
//! whether a prompt change made the model better at the work - only running
//! the work can, and only numbers make two runs comparable. This module holds
//! the parts of that which are use-case logic and nothing else:
//!
//! - [`task`] - the suite document and its validation
//! - [`verify`] - deciding whether a task succeeded, through the ports
//! - [`metrics`] - an [`EventSink`](agent_domain::ports::events::EventSink)
//!   that counts what the loop did
//! - [`report`] - the per-attempt record, the aggregation and the fingerprint
//!
//! What is *not* here: copying fixtures, starting sandboxes, timing the wall
//! clock and printing. Those need a filesystem, a runtime or a terminal, and
//! live in `agent-infrastructure` and `agent-cli` (invariant §1).

pub mod metrics;
pub mod report;
pub mod task;
pub mod verify;

pub use metrics::{MetricsSink, RunMetrics};
pub use report::{
    AttemptInput, AttemptReport, Fingerprint, SuiteReport, Summary, TaskReport, Tokens, Totals,
    fnv1a64,
};
pub use task::{Check, PromptSource, SuiteError, Task, TaskId, TaskSuite};
pub use verify::{CheckOutcome, verify};
