//! The agent: a loop, the dispatcher it drives, the context that feeds it, and
//! the session it mutates.

pub mod config;
pub mod dispatch;
pub mod loop_runner;
pub mod prompt;
pub mod session;

pub use config::AgentLoopConfig;
pub use dispatch::{DispatchConfig, ToolDispatcher};
pub use loop_runner::{AgentDependencies, AgentLoop, AgentOutcome};
pub use prompt::{AppendingPromptBuilder, DefaultPromptBuilder, FixedPromptBuilder};
pub use session::Session;
