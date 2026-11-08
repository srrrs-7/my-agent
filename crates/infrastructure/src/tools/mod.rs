//! Tool decorators.
//!
//! Tools themselves are use cases and live in `agent-application`. What lives
//! here is the cross-cutting behaviour that needs a runtime - which is exactly
//! why the application layer can stay executor independent - or that needs to
//! know this process's configuration, such as which values are secrets.

pub mod redact;
pub mod timeout;

pub use redact::{RedactingTool, Redactor};
pub use timeout::TimeoutTool;
