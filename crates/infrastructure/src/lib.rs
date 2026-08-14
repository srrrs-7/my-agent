//! # agent-infrastructure
//!
//! Adapters. Everything here implements a port from [`agent_domain::ports`] and
//! is the only place allowed to know about HTTP, tokio, the real filesystem,
//! environment variables and vendor payload shapes.
//!
//! Nothing in this crate depends on `agent-application`: the dependency arrow
//! points inwards only, which is what lets the use cases be tested with fakes.

pub mod config;
pub mod exec;
pub mod fs;
pub mod llm;
pub(crate) mod net;
pub mod telemetry;
pub mod tools;
pub mod web;

pub use config::{ConfigError, Settings};
