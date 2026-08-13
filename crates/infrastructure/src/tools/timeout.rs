//! Wraps any tool with a wall-clock limit.
//!
//! Without this, one pathological call (a regex that backtracks, a huge tree
//! walk) hangs the agent with no feedback. With it, the model receives a
//! `tool_result` explaining the timeout and can narrow its request.

use std::sync::Arc;
use std::time::Duration;

use agent_domain::error::ToolError;
use agent_domain::model::tool::{ToolDefinition, ToolOutcome};
use agent_domain::ports::tool::Tool;
use async_trait::async_trait;
use serde_json::Value;

pub struct TimeoutTool {
    inner: Arc<dyn Tool>,
    timeout: Duration,
}

impl TimeoutTool {
    pub fn new(inner: Arc<dyn Tool>, timeout: Duration) -> Self {
        Self { inner, timeout }
    }

    /// Convenience for the composition root.
    pub fn wrap(inner: Arc<dyn Tool>, timeout: Duration) -> Arc<dyn Tool> {
        Arc::new(Self::new(inner, timeout))
    }
}

#[async_trait]
impl Tool for TimeoutTool {
    fn definition(&self) -> ToolDefinition {
        self.inner.definition()
    }

    async fn execute(&self, arguments: Value) -> Result<ToolOutcome, ToolError> {
        match tokio::time::timeout(self.timeout, self.inner.execute(arguments)).await {
            Ok(result) => result,
            Err(_) => Err(ToolError::Timeout {
                tool: self.inner.definition().name,
                seconds: self.timeout.as_secs(),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_domain::model::tool::{ToolName, ToolSafety};
    use serde_json::json;

    struct SlowTool;

    #[async_trait]
    impl Tool for SlowTool {
        fn definition(&self) -> ToolDefinition {
            ToolDefinition {
                name: ToolName::new("slow").unwrap(),
                description: "sleeps".into(),
                input_schema: json!({"type": "object"}),
                safety: ToolSafety::ReadOnly,
            }
        }

        async fn execute(&self, _arguments: Value) -> Result<ToolOutcome, ToolError> {
            tokio::time::sleep(Duration::from_secs(30)).await;
            Ok(ToolOutcome::new("never"))
        }
    }

    #[tokio::test]
    async fn a_hanging_tool_becomes_a_timeout_error() {
        let tool = TimeoutTool::new(Arc::new(SlowTool), Duration::from_millis(20));
        let error = tool.execute(json!({})).await.unwrap_err();
        assert!(matches!(error, ToolError::Timeout { .. }), "got {error:?}");
    }
}
