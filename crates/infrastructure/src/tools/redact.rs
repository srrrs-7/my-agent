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
//! Only exact occurrences are caught. An encoded, split or truncated secret
//! passes through, and so does any secret this process was never given. This
//! narrows the leak; it does not close it.

use std::borrow::Cow;
use std::fmt;
use std::sync::Arc;

use agent_domain::error::ToolError;
use agent_domain::model::tool::{PlannedWrite, ToolDefinition, ToolOutcome};
use agent_domain::ports::tool::Tool;
use async_trait::async_trait;
use serde_json::Value;

/// What a secret is replaced with.
pub const REDACTED: &str = "[REDACTED]";

/// Values shorter than this are not treated as secrets: they would also
/// match ordinary text, and a redactor that mangles every third word gets
/// turned off.
///
/// This is the floor for secrets the operator configured. Values merely
/// *guessed* to be secrets from a variable's name face a stricter test before
/// they get here (`exec::env::secret_values`), because a guess is often an
/// ordinary word.
pub const MIN_SECRET_CHARS: usize = 8;

/// A set of known secret values and the replacement of them in text.
///
/// `Debug` shows how many there are, never what they are.
#[derive(Clone, Default)]
pub struct Redactor {
    secrets: Vec<String>,
}

impl Redactor {
    pub fn new(secrets: impl IntoIterator<Item = String>) -> Self {
        let mut secrets: Vec<String> = secrets
            .into_iter()
            .filter(|secret| secret.chars().count() >= MIN_SECRET_CHARS)
            .collect();
        secrets.sort();
        secrets.dedup();
        Self { secrets }
    }

    pub fn len(&self) -> usize {
        self.secrets.len()
    }

    pub fn is_empty(&self) -> bool {
        self.secrets.is_empty()
    }

    /// `text` with every occurrence of every secret replaced by [`REDACTED`].
    ///
    /// All matches are found before anything is replaced, and overlapping
    /// ones merged, so two secrets sharing characters are both hidden whole -
    /// replacing them one after the other would leave the tail of the second.
    pub fn redact<'a>(&self, text: &'a str) -> Cow<'a, str> {
        let mut found: Vec<(usize, usize)> = self
            .secrets
            .iter()
            .flat_map(|secret| {
                text.match_indices(secret.as_str())
                    .map(|(start, matched)| (start, start + matched.len()))
            })
            .collect();
        if found.is_empty() {
            return Cow::Borrowed(text);
        }
        found.sort_unstable();

        // Overlapping or touching matches become one span.
        let mut spans: Vec<(usize, usize)> = Vec::with_capacity(found.len());
        for (start, end) in found {
            match spans.last_mut() {
                Some(last) if start <= last.1 => last.1 = last.1.max(end),
                _ => spans.push((start, end)),
            }
        }

        // Match boundaries are character boundaries: a valid UTF-8 needle
        // found in valid UTF-8 starts and ends on one.
        let mut redacted = String::with_capacity(text.len());
        let mut copied = 0;
        for (start, end) in spans {
            redacted.push_str(&text[copied..start]);
            redacted.push_str(REDACTED);
            copied = end;
        }
        redacted.push_str(&text[copied..]);
        Cow::Owned(redacted)
    }
}

impl fmt::Debug for Redactor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Redactor({} secrets)", self.secrets.len())
    }
}

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

    #[test]
    fn every_occurrence_is_replaced_and_the_rest_is_kept() {
        let redactor = Redactor::new([KEY.to_string()]);
        assert_eq!(
            redactor.redact(&format!("a {KEY} b {KEY}{KEY} c")),
            "a [REDACTED] b [REDACTED] c"
        );
    }

    #[test]
    fn text_without_a_secret_is_returned_as_is() {
        let redactor = Redactor::new([KEY.to_string()]);
        assert!(matches!(
            redactor.redact("nothing to hide"),
            Cow::Borrowed("nothing to hide")
        ));
        assert!(matches!(
            Redactor::default().redact("anything"),
            Cow::Borrowed(_)
        ));
    }

    #[test]
    fn short_values_are_not_treated_as_secrets() {
        let redactor = Redactor::new(["topic".to_string(), "1234567".to_string()]);
        assert!(redactor.is_empty());
        assert_eq!(redactor.redact("topic 1234567"), "topic 1234567");
    }

    #[test]
    fn overlapping_secrets_are_hidden_whole() {
        let redactor = Redactor::new([
            "abcdefgh-1234".to_string(),
            "1234-ijklmnop".to_string(),
            "cdefgh-1".to_string(),
        ]);
        assert_eq!(
            redactor.redact("x abcdefgh-1234-ijklmnop y"),
            "x [REDACTED] y"
        );
    }

    #[test]
    fn non_ascii_text_around_a_secret_survives() {
        let redactor = Redactor::new([KEY.to_string()]);
        assert_eq!(
            redactor.redact(&format!("鍵は{KEY}です")),
            "鍵は[REDACTED]です"
        );
    }

    #[test]
    fn debug_output_does_not_show_the_secrets() {
        let shown = format!("{:?}", Redactor::new([KEY.to_string()]));
        assert_eq!(shown, "Redactor(1 secrets)");
    }
}
