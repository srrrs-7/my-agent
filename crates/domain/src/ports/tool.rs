//! The tool port.

use async_trait::async_trait;
use serde_json::Value;

use crate::error::ToolError;
use crate::model::tool::{ToolDefinition, ToolOutcome};

/// A capability the agent can offer to the model.
///
/// Implementations live in the application layer (they are use cases), receive
/// their dependencies through other ports, and never talk to the network or the
/// filesystem directly.
#[async_trait]
pub trait Tool: Send + Sync {
    fn definition(&self) -> ToolDefinition;

    /// `arguments` is the raw JSON produced by the model; implementations are
    /// responsible for validating it and returning
    /// [`ToolError::InvalidInput`] with a message the *model* can act on.
    async fn execute(&self, arguments: Value) -> Result<ToolOutcome, ToolError>;
}
