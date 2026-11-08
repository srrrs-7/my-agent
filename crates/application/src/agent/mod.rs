//! The agent: a loop, the dispatcher it drives, the context that feeds it, and
//! the session it mutates.

pub mod compaction;
pub mod config;
pub mod dispatch;
pub mod interrupt;
pub mod loop_runner;
mod plan;
pub mod prompt;
pub mod session;

pub use compaction::{CompactionConfig, CompactionReport, HistoryCompactor};
pub use config::AgentLoopConfig;
pub use dispatch::{DispatchConfig, ToolDispatcher};
pub use interrupt::Interrupt;
pub use loop_runner::{AgentDependencies, AgentLoop, AgentOutcome};
pub use prompt::{AppendingPromptBuilder, DefaultPromptBuilder, FixedPromptBuilder};
pub use session::Session;
