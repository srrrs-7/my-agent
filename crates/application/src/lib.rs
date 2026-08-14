//! # agent-application
//!
//! The use-case layer. It orchestrates domain objects through ports and knows
//! *what* the agent does, never *how* the outside world is reached.
//!
//! Allowed dependencies: [`agent_domain`] plus runtime-agnostic helpers.
//! In particular there is no `tokio` here - concerns that need a runtime
//! (timeouts, retries, HTTP) are implemented as decorators in
//! `agent-infrastructure`, so this crate stays executor independent and
//! trivially testable.

pub mod agent;
pub mod error;
pub mod tools;

pub use agent::{
    AgentDependencies, AgentLoop, AgentLoopConfig, AgentOutcome, DefaultPromptBuilder, Session,
    prompt::build_system_prompt,
};
pub use error::AppError;
pub use tools::registry::ToolRegistry;
