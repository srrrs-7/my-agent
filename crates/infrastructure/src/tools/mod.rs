//! Tool decorators.
//!
//! Tools themselves are use cases and live in `agent-application`. What lives
//! here is the cross-cutting behaviour that needs a runtime - which is exactly
//! why the application layer can stay executor independent.

pub mod timeout;

pub use timeout::TimeoutTool;
