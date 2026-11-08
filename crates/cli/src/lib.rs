//! `agent` - a sandboxed, loop-driven coding agent.
//!
//! Layer map (dependencies point inwards only):
//!
//! ```text
//!   agent-cli ............ this crate: argv, rendering, composition root
//!     -> agent-application  the loop, the tools, prompt assembly
//!     -> agent-infrastructure  HTTP clients, filesystem, config
//!          -> agent-domain     entities, value objects, ports
//! ```
//!
//! A library with a thin binary (`main.rs`) on top, so that the tests under
//! `tests/` can build the agent through the real composition root instead of
//! re-wiring it by hand - a wiring mistake is invisible to a test that does
//! its own wiring. Nothing outside this package depends on it.

mod approval;
pub mod args;
pub mod commands;
pub mod composition;
mod ctrl_c;
mod input;
mod notify;
mod render;

/// Fresh identifier for a conversation.
///
/// It names the session's log file, so it has to stay unique across runs and
/// not merely within a process - which is what rules out a counter.
pub(crate) fn session_id() -> String {
    uuid::Uuid::new_v4().to_string()
}
