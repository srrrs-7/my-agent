//! The one error type the stable API returns.

use std::fmt;

/// Why the agent could not be built, or a turn could not be run.
///
/// Opaque on purpose: what can go wrong grows with every adapter, and an
/// embedder that matched on it would break with each one. `Display` prints
/// the whole chain on one line ("cannot build the LLM provider: ..."), which
/// is written to be shown to the person who configured the agent.
pub struct Error {
    inner: anyhow::Error,
}

impl Error {
    pub(crate) fn new(inner: impl Into<anyhow::Error>) -> Self {
        Self {
            inner: inner.into(),
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{:#}", self.inner)
    }
}

impl fmt::Debug for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&self.inner, formatter)
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.inner.source()
    }
}
