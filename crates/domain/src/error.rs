//! Error types owned by the domain.
//!
//! Ports return these, so adapters must translate their vendor-specific
//! failures (reqwest, std::io, ...) into domain vocabulary at the boundary.

use thiserror::Error;

use crate::model::access::FileAccess;
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

/// Why a [`crate::model::plan::Plan`] was refused.
///
/// Items are named by their 1-based position, the way the model counts them in
/// the list it just wrote. Each message says what to change, because the one
/// reader who can act on it is the model that sent the plan.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum PlanError {
    #[error(
        "the plan has {count} items, more than the limit of {limit}. Merge small steps into \
         larger ones"
    )]
    TooManyItems { count: usize, limit: usize },

    #[error("item {number} is blank. Every item needs a short description of the step")]
    BlankItem { number: usize },

    #[error("item {number} spans several lines. Write each step as a single line")]
    MultiLineItem { number: usize },

    #[error(
        "item {number} is {bytes} bytes, more than the limit of {limit}. Shorten it to one \
         brief line"
    )]
    ItemTooLong {
        number: usize,
        bytes: usize,
        limit: usize,
    },

    #[error(
        "items {} are all `in_progress`, but only one step can be in progress at a time. Mark \
         the others `pending` or `completed`",
        list_numbers(.numbers)
    )]
    SeveralInProgress { numbers: Vec<usize> },
}

fn list_numbers(numbers: &[usize]) -> String {
    numbers
        .iter()
        .map(usize::to_string)
        .collect::<Vec<_>>()
        .join(", ")
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

    /// The workspace's [`crate::model::access::AccessPolicy`] refused it.
    /// `path` is the one the caller asked for, never where a symlink led:
    /// the model learns that the name is off limits, not where secrets live.
    #[error(
        "`{path}` is protected by the operator's access policy and cannot be {}",
        refusal(.access)
    )]
    AccessDenied { path: String, access: FileAccess },

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

/// The rest of an [`FsError::AccessDenied`] message. Written for the model:
/// what to do instead, and not to go looking for another way in - a model
/// that means well would otherwise reach for `run_command`.
fn refusal(access: &FileAccess) -> &'static str {
    match access {
        FileAccess::Read => {
            "read. Do not try to reach it another way; if you need a value from it, ask the user."
        }
        FileAccess::Write => {
            "written. Do not try to change it another way; if the change is needed, ask the \
             user to make it."
        }
    }
}

impl From<DomainError> for FsError {
    fn from(value: DomainError) -> Self {
        match value {
            DomainError::PathEscape { path } => FsError::OutsideWorkspace { path },
            other => FsError::InvalidPattern(other.to_string()),
        }
    }
}

/// Failures produced by [`crate::ports::web::WebFetcher`] implementations.
///
/// `InvalidUrl` and `Blocked` mean the *model's* URL was unacceptable - tools
/// map them to invalid-input so the model picks another URL. The rest are
/// execution failures of an acceptable request.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum FetchError {
    #[error("`{url}` is not a fetchable URL: {reason}")]
    InvalidUrl { url: String, reason: String },

    #[error("fetching `{url}` is not allowed: {reason}")]
    Blocked { url: String, reason: String },

    #[error("transport error fetching `{url}`: {message}")]
    Transport { url: String, message: String },

    #[error("fetching `{url}` timed out after {seconds}s")]
    Timeout { url: String, seconds: u64 },

    #[error("`{url}` answered with HTTP {status}")]
    HttpStatus { url: String, status: u16 },

    #[error("`{url}` returned {content_type}, which is not text and cannot be shown")]
    UnsupportedContent { url: String, content_type: String },
}

/// Failures produced by [`crate::ports::command::CommandRunner`].
///
/// A command that runs and *fails* is not an error here - a non-zero exit is
/// ordinary output the model needs to see. These are the cases where the
/// command never produced a usable result at all.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum CommandError {
    /// The request was refused before anything ran (empty command, a cwd that
    /// does not exist).
    #[error("the command was not run: {reason}")]
    Refused { reason: String },

    /// The sandbox could not be established. Deliberately fatal: falling back
    /// to an unconfined run would silently remove the protection the operator
    /// asked for.
    #[error("the sandbox could not be applied: {reason}")]
    SandboxUnavailable { reason: String },

    #[error("the command could not be started: {message}")]
    SpawnFailed { message: String },

    #[error("the command was killed after {seconds}s")]
    TimedOut { seconds: u64 },

    #[error("io error while running the command: {message}")]
    Io { message: String },
}

/// Failures produced by [`crate::ports::tool_server::ToolServer`].
///
/// A tool that ran and reported failure is not one of these - that comes back
/// as an output with `is_error` set. These are the cases where no usable
/// answer arrived. Messages are read by the model, so each says whether
/// trying again can help.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum ToolServerError {
    /// The server process is gone or its connection is closed. Every later
    /// call fails the same way; servers are not restarted mid-session.
    #[error(
        "the tool server `{server}` is no longer running ({reason}). Its tools cannot be used \
         for the rest of this session; do not retry them"
    )]
    Stopped { server: String, reason: String },

    /// The server answered the call with a protocol-level error.
    #[error("the tool server `{server}` refused the call: {message}")]
    Refused { server: String, message: String },

    /// The server answered with something that is not a valid result.
    #[error("the tool server `{server}` sent an invalid response: {reason}")]
    Protocol { server: String, reason: String },
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

/// Failures produced by [`crate::ports::notify::Notifier`] implementations.
///
/// None of the variants carries the destination. A webhook URL is a
/// credential: for Slack and for a public ntfy topic, knowing it is all it
/// takes to post or to read. These errors end up in logs.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum NotifyError {
    #[error("the notification endpoint could not be reached: {message}")]
    Transport { message: String },

    #[error("the notification endpoint did not answer within {seconds}s")]
    Timeout { seconds: u64 },

    #[error("the notification endpoint answered with HTTP {status}")]
    HttpStatus { status: u16 },
}
