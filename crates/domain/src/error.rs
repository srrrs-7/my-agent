//! Error types owned by the domain.
//!
//! Ports return these, so adapters must translate their vendor-specific
//! failures (reqwest, std::io, ...) into domain vocabulary at the boundary.

use thiserror::Error;

use crate::model::tool::ToolName;

/// Invariant violations detected while constructing domain values.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum DomainError {
    #[error("invalid {kind}: {reason}")]
    Invalid { kind: &'static str, reason: String },

    #[error("path `{path}` escapes the workspace root")]
    PathEscape { path: String },
}

impl DomainError {
    pub fn invalid(kind: &'static str, reason: impl Into<String>) -> Self {
        Self::Invalid {
            kind,
            reason: reason.into(),
        }
    }
}

/// Failures produced by [`crate::ports::llm::LlmProvider`] implementations.
#[derive(Debug, Error, Clone)]
pub enum LlmError {
    #[error("transport error: {0}")]
    Transport(String),

    #[error("request timed out after {seconds}s")]
    Timeout { seconds: u64 },

    #[error("authentication failed: {0}")]
    Auth(String),

    #[error("rate limited{}", .retry_after_secs.map(|s| format!(" (retry after {s}s)")).unwrap_or_default())]
    RateLimited { retry_after_secs: Option<u64> },

    #[error("provider returned HTTP {status}: {message}")]
    Api { status: u16, message: String },

    #[error("could not decode provider response: {0}")]
    InvalidResponse(String),

    #[error("provider `{provider}` does not support {feature}")]
    Unsupported { provider: String, feature: String },

    #[error("no route for this request: {0}")]
    NoRoute(String),

    #[error("configuration error: {0}")]
    Configuration(String),
}

impl LlmError {
    /// Whether retrying the exact same request could plausibly succeed.
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::Transport(_) | Self::Timeout { .. } | Self::RateLimited { .. } => true,
            Self::Api { status, .. } => *status >= 500 || *status == 408 || *status == 429,
            _ => false,
        }
    }
}

/// Failures produced by filesystem ports.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum FsError {
    #[error("no such file or directory: {path}")]
    NotFound { path: String },

    #[error("`{path}` is not a regular file")]
    NotAFile { path: String },

    #[error("`{path}` is not a directory")]
    NotADirectory { path: String },

    #[error("`{path}` already exists")]
    AlreadyExists { path: String },

    #[error("permission denied: {path}")]
    PermissionDenied { path: String },

    #[error("`{path}` resolves outside the workspace root and was refused")]
    OutsideWorkspace { path: String },

    #[error("`{path}` is {actual} bytes which exceeds the {limit} byte limit")]
    TooLarge {
        path: String,
        actual: u64,
        limit: u64,
    },

    #[error("`{path}` is not valid UTF-8 text")]
    NotUtf8 { path: String },

    #[error("invalid pattern: {0}")]
    InvalidPattern(String),

    #[error("io error on `{path}`: {message}")]
    Io { path: String, message: String },
}

impl From<DomainError> for FsError {
    fn from(value: DomainError) -> Self {
        match value {
            DomainError::PathEscape { path } => FsError::OutsideWorkspace { path },
            other => FsError::InvalidPattern(other.to_string()),
        }
    }
}

/// Failures produced while running a tool.
#[derive(Debug, Error, Clone)]
pub enum ToolError {
    #[error("unknown tool `{0}`")]
    NotFound(ToolName),

    #[error("invalid arguments for `{tool}`: {reason}")]
    InvalidInput { tool: ToolName, reason: String },

    #[error("tool `{tool}` failed: {reason}")]
    Execution { tool: ToolName, reason: String },

    #[error("tool `{tool}` was rejected: {reason}")]
    Rejected { tool: ToolName, reason: String },

    #[error("tool `{tool}` timed out after {seconds}s")]
    Timeout { tool: ToolName, seconds: u64 },
}

impl ToolError {
    pub fn invalid_input(tool: &ToolName, reason: impl Into<String>) -> Self {
        Self::InvalidInput {
            tool: tool.clone(),
            reason: reason.into(),
        }
    }

    pub fn execution(tool: &ToolName, reason: impl Into<String>) -> Self {
        Self::Execution {
            tool: tool.clone(),
            reason: reason.into(),
        }
    }
}

/// Failures produced by the human-in-the-loop approval port.
#[derive(Debug, Error, Clone)]
pub enum ApprovalError {
    #[error("approval channel is unavailable: {0}")]
    Unavailable(String),
}
