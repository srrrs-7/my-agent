use agent_domain::error::{FsError, LlmError};
use thiserror::Error;

/// Failures that abort a whole run.
///
/// Deliberately narrow: only a broken provider or an unreadable workspace can
/// abort. A failing tool call, a denied approval or an invalid model argument
/// all become error-flagged `tool_result`s and go back to the model, because
/// the model recovering from its own mistake is the normal case, not an
/// exception. If a new variant is about to be added here, first ask whether
/// the failure should instead be fed back into the loop.
#[derive(Debug, Error)]
pub enum AppError {
    #[error(transparent)]
    Llm(#[from] LlmError),

    #[error(transparent)]
    FileSystem(#[from] FsError),
}
