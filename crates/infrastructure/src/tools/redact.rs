//! Hides the secrets this process holds in whatever a tool returns.
//!
//! The access policy keeps the file tools off `.env`. It cannot keep a secret
//! out of every file: `run_command` runs with the filesystem readable (Landlock
//! cannot carve `.env` out of a readable tree), a key gets pasted into a
//! config file or a log, and a hard link gives `.env` a second name that no
//! rule knows. So the values themselves are a second line. The secrets the
//! agent was configured with - provider API keys, the webhook URL - are
//! replaced before a tool's output reaches the model, the session log, or a
//! notification excerpt.
//!
//! What counts as a secret, and the replacing itself, is
//! [`crate::secrets`]; this is the decorator that applies it to tools.

use std::borrow::Cow;
use std::sync::Arc;

use agent_domain::error::ToolError;
use agent_domain::model::tool::{PlannedWrite, ToolDefinition, ToolOutcome};
use agent_domain::ports::tool::Tool;
use async_trait::async_trait;
use serde_json::Value;

use crate::secrets::Redactor;

/// Wraps a tool so that nothing it returns - output, summary or error -
/// carries a known secret.
pub struct RedactingTool {
    inner: Arc<dyn Tool>,
    redactor: Arc<Redactor>,
}

impl RedactingTool {
    pub fn new(inner: Arc<dyn Tool>, redactor: Arc<Redactor>) -> Self {
        Self { inner, redactor }
    }

    /// Convenience for the composition root.
    pub fn wrap(inner: Arc<dyn Tool>, redactor: Arc<Redactor>) -> Arc<dyn Tool> {
        Arc::new(Self::new(inner, redactor))
    }

    fn scrub(&self, text: String) -> String {
        match self.redactor.redact(&text) {
            Cow::Borrowed(_) => text,
            Cow::Owned(redacted) => redacted,
        }
    }

    /// Error texts can quote what the model sent or what a command printed.
    /// Every variant is listed so that a new one carrying text cannot slip
    /// past unnoticed.
    fn scrub_error(&self, error: ToolError) -> ToolError {
        match error {
            ToolError::NotFound(name) => ToolError::NotFound(name),
            ToolError::InvalidInput { tool, reason } => ToolError::InvalidInput {
                tool,
                reason: self.scrub(reason),
            },
            ToolError::Execution { tool, reason } => ToolError::Execution {
                tool,
                reason: self.scrub(reason),
            },
            ToolError::Rejected { tool, reason } => ToolError::Rejected {
                tool,
                reason: self.scrub(reason),
            },
            ToolError::Timeout { tool, seconds } => ToolError::Timeout { tool, seconds },
        }
    }
}

#[async_trait]
impl Tool for RedactingTool {
    fn definition(&self) -> ToolDefinition {
        self.inner.definition()
    }

    /// Passed through untouched: a preview goes to the dependency approval
    /// prompt on this machine's terminal, never to the model, and a secret
    /// replaced in it would make the shown change differ from the written one.
    async fn preview(&self, arguments: &Value) -> Result<Vec<PlannedWrite>, ToolError> {
        self.inner.preview(arguments).await
    }

    async fn execute(&self, arguments: Value) -> Result<ToolOutcome, ToolError> {
        match self.inner.execute(arguments).await {
            Ok(outcome) => Ok(ToolOutcome {
                content: self.scrub(outcome.content),
                summary: outcome.summary.map(|summary| self.scrub(summary)),
            }),
            Err(error) => Err(self.scrub_error(error)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::secrets::REDACTED;
    use agent_domain::model::tool::{ToolName, ToolSafety};
    use serde_json::json;

    const KEY: &str = "sk-ant-api03-0123456789abcdef";

    /// Returns whatever it is told to, as output or as an error.
    struct Echo {
        fail: bool,
    }

    #[async_trait]
    impl Tool for Echo {
        fn definition(&self) -> ToolDefinition {
            ToolDefinition {
                name: ToolName::new("echo").unwrap(),
                description: "echoes".into(),
                input_schema: json!({"type": "object"}),
                safety: ToolSafety::ReadOnly,
            }
        }

        async fn preview(&self, _arguments: &Value) -> Result<Vec<PlannedWrite>, ToolError> {
            Ok(Vec::new())
        }

        async fn execute(&self, arguments: Value) -> Result<ToolOutcome, ToolError> {
            let text = arguments["text"].as_str().unwrap_or_default().to_string();
            if self.fail {
                return Err(ToolError::execution(&self.definition().name, text));
            }
            Ok(ToolOutcome::new(text.clone()).with_summary(text))
        }
    }

    fn redacting(fail: bool) -> RedactingTool {
        RedactingTool::new(
            Arc::new(Echo { fail }),
            Arc::new(Redactor::new([KEY.to_string()])),
        )
    }

    #[tokio::test]
    async fn a_known_secret_never_reaches_the_model_in_output_or_summary() {
        let outcome = redacting(false)
            .execute(json!({"text": format!("$ cat .env\nAGENT_API_KEY={KEY}\n")}))
            .await
            .unwrap();

        assert_eq!(outcome.content, "$ cat .env\nAGENT_API_KEY=[REDACTED]\n");
        assert!(
            !outcome.summary.as_deref().unwrap().contains(KEY),
            "{outcome:?}"
        );
    }

    #[tokio::test]
    async fn a_known_secret_in_an_error_is_hidden_too() {
        let error = redacting(true)
            .execute(json!({"text": format!("command failed: bad token {KEY}")}))
            .await
            .unwrap_err();
        let message = error.to_string();
        assert!(!message.contains(KEY), "{message}");
        assert!(message.contains(REDACTED), "{message}");
    }
}
