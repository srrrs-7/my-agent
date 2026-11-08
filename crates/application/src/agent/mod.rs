//! The agent: a loop, the dispatcher it drives, the context that feeds it, and
//! the session it mutates.

pub mod call_scope;
pub mod compaction;
pub mod config;
pub mod context_report;
pub mod dispatch;
pub mod interrupt;
pub mod loop_runner;
mod plan;
pub mod prompt;
pub mod references;
pub mod session;

pub use compaction::{CompactionConfig, CompactionReport, HistoryCompactor, ManualCompaction};
pub use config::AgentLoopConfig;
pub use context_report::{ContextReport, PlanNote};
pub use dispatch::{DispatchConfig, Dispatched, ToolDispatcher};
pub use interrupt::Interrupt;
pub use loop_runner::{AgentDependencies, AgentLoop, AgentOutcome};
pub use prompt::{
    AppendingPromptBuilder, DefaultPromptBuilder, FixedPromptBuilder, SubagentPromptBuilder,
};
pub use references::{Attachments, ReferenceLimits, attach_references};
pub use session::Session;
