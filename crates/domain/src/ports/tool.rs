//! The tool port.

use async_trait::async_trait;
use serde_json::Value;

use crate::error::ToolError;
use crate::model::tool::{PlannedWrite, ToolDefinition, ToolOutcome};

/// A capability the agent can offer to the model.
///
/// Implementations live in the application layer (they are use cases), receive
/// their dependencies through other ports, and never talk to the network or the
/// filesystem directly.
#[async_trait]
pub trait Tool: Send + Sync {
    fn definition(&self) -> ToolDefinition;

    /// The files `arguments` would make this tool write, and what each would
    /// contain afterwards - without writing anything.
    ///
    /// Tools that write no files through the workspace (readers, commands,
    /// network calls) return an empty list. There is deliberately no default:
    /// a decorator that forgot to forward this would otherwise answer "no
    /// writes" for the tool it wraps, and every write through it would skip
    /// the dependency approval without a trace. Without a default, the
    /// omission is a compile error.
    ///
    /// An error means "could not tell". [`ToolError::InvalidInput`] is the one
    /// the dispatcher lets through to `execute`, which refuses the same
    /// arguments before writing anything; for any other error the call is not
    /// run, since nothing is known about what it would write. So return
    /// `InvalidInput` only for arguments `execute` is certain to reject.
    async fn preview(&self, arguments: &Value) -> Result<Vec<PlannedWrite>, ToolError>;

    /// `arguments` is the raw JSON produced by the model; implementations are
    /// responsible for validating it and returning
    /// [`ToolError::InvalidInput`] with a message the *model* can act on.
    async fn execute(&self, arguments: Value) -> Result<ToolOutcome, ToolError>;
}
