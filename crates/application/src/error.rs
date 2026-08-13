use agent_domain::error::{ApprovalError, DomainError, FsError, LlmError, ToolError};
use thiserror::Error;

/// Failures that abort a whole run.
///
/// Note what is *not* here: a failing tool call. Those are turned into an
/// error-flagged `tool_result` and handed back to the model, because the model
/// recovering from its own mistake is the normal case, not an exception.
#[derive(Debug, Error)]
pub enum AppError {
    #[error(transparent)]
    Llm(#[from] LlmError),

    #[error(transparent)]
    FileSystem(#[from] FsError),

    #[error(transparent)]
    Tool(#[from] ToolError),

    #[error(transparent)]
    Approval(#[from] ApprovalError),

    #[error(transparent)]
    Domain(#[from] DomainError),

    #[error("configuration error: {0}")]
    Configuration(String),
}
