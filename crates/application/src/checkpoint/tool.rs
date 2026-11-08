//! The decorator that feeds the journal.

use std::sync::Arc;

use agent_domain::error::ToolError;
use agent_domain::model::tool::{PlannedWrite, ToolDefinition, ToolOutcome};
use agent_domain::ports::tool::Tool;
use async_trait::async_trait;
use serde_json::Value;

use super::CheckpointJournal;

/// Records what a call is about to write, right before it runs.
///
/// Asks the tool it wraps for its [`Tool::preview`] at the last moment rather
/// than reusing the one the dispatcher took for the approval prompt: the
/// human may have taken minutes to answer, and the text a restore puts back
/// has to be what the file held when it was overwritten.
///
/// A call that plans no writes is recorded by name only - it is a command or
/// a remote tool whose effects cannot be read off its arguments, and the user
/// is told it was not undone.
pub struct CheckpointingTool {
    inner: Arc<dyn Tool>,
    journal: Arc<CheckpointJournal>,
}

impl CheckpointingTool {
    pub fn wrap(inner: Arc<dyn Tool>, journal: Arc<CheckpointJournal>) -> Arc<dyn Tool> {
        Arc::new(Self { inner, journal })
    }
}

#[async_trait]
impl Tool for CheckpointingTool {
    fn definition(&self) -> ToolDefinition {
        self.inner.definition()
    }

    async fn preview(&self, arguments: &Value) -> Result<Vec<PlannedWrite>, ToolError> {
        self.inner.preview(arguments).await
    }

    async fn execute(&self, arguments: Value) -> Result<ToolOutcome, ToolError> {
        match self.inner.preview(&arguments).await {
            Ok(writes) if writes.is_empty() => {
                self.journal.record_untracked(self.inner.definition().name);
            }
            Ok(writes) => self.journal.record_writes(writes),
            // `execute` refuses these arguments before writing anything (the
            // contract of `preview`), so there is nothing to record.
            Err(ToolError::InvalidInput { .. }) => {}
            // What the call would write is unknown, so it could not be
            // undone: refused, as the dispatcher refuses it for the same
            // reason.
            Err(error) => return Err(error),
        }
        self.inner.execute(arguments).await
    }
}
