//! Tool vocabulary: how the agent advertises capabilities to the model and how
//! the model asks for them.

use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::DomainError;

/// A validated tool name.
///
/// Both the OpenAI and the Anthropic APIs restrict function names to
/// `[a-zA-Z0-9_-]{1,64}`, so the constraint belongs to the domain rather than
/// to any single adapter.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ToolName(String);

impl ToolName {
    pub fn new(raw: impl Into<String>) -> Result<Self, DomainError> {
        let raw = raw.into();
        if raw.is_empty() || raw.len() > 64 {
            return Err(DomainError::invalid(
                "tool name",
                "must be 1..=64 characters",
            ));
        }
        if !raw
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        {
            return Err(DomainError::invalid(
                "tool name",
                format!("`{raw}` may only contain [a-zA-Z0-9_-]"),
            ));
        }
        Ok(Self(raw))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for ToolName {
    type Error = DomainError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<ToolName> for String {
    fn from(value: ToolName) -> Self {
        value.0
    }
}

impl fmt::Display for ToolName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Identifier correlating a [`ToolCall`] with its [`ToolResult`].
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ToolCallId(String);

impl ToolCallId {
    pub fn new(raw: impl Into<String>) -> Self {
        Self(raw.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ToolCallId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// How much damage a tool can do. Drives the approval policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolSafety {
    /// Observes state only. Safe to run without asking and safe to parallelise.
    ReadOnly,
    /// Changes workspace state in a recoverable way (create / edit a file).
    Mutating,
    /// Sends data outside the process (a URL is an outbound message: its path
    /// and query carry whatever the model put there). Not read-only even when
    /// nothing local changes, so the default policy always confirms it and the
    /// human sees the full destination before anything leaves.
    Network,
    /// Irreversible or reaches outside the workspace. Always confirmed.
    Destructive,
}

impl ToolSafety {
    pub fn is_read_only(self) -> bool {
        matches!(self, Self::ReadOnly)
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::ReadOnly => "read-only",
            Self::Mutating => "mutating",
            Self::Network => "network",
            Self::Destructive => "destructive",
        }
    }
}

/// What the model is told about a tool.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: ToolName,
    pub description: String,
    /// JSON Schema describing the accepted arguments.
    pub input_schema: Value,
    pub safety: ToolSafety,
}

/// A request from the model to invoke a tool.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: ToolCallId,
    pub name: ToolName,
    pub arguments: Value,
}

impl ToolCall {
    pub fn new(id: ToolCallId, name: ToolName, arguments: Value) -> Self {
        Self {
            id,
            name,
            arguments,
        }
    }
}

/// The outcome handed back to the model.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolResult {
    pub call_id: ToolCallId,
    pub tool_name: ToolName,
    pub content: String,
    pub is_error: bool,
}

impl ToolResult {
    pub fn ok(call: &ToolCall, content: impl Into<String>) -> Self {
        Self {
            call_id: call.id.clone(),
            tool_name: call.name.clone(),
            content: content.into(),
            is_error: false,
        }
    }

    pub fn error(call: &ToolCall, content: impl Into<String>) -> Self {
        Self {
            call_id: call.id.clone(),
            tool_name: call.name.clone(),
            content: content.into(),
            is_error: true,
        }
    }
}

/// What a tool produced. Kept separate from [`ToolResult`] so that a tool
/// implementation never has to know about call ids.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ToolOutcome {
    /// Text handed back to the model.
    pub content: String,
    /// Short single-line summary for the terminal UI.
    pub summary: Option<String>,
}

impl ToolOutcome {
    pub fn new(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            summary: None,
        }
    }

    pub fn with_summary(mut self, summary: impl Into<String>) -> Self {
        self.summary = Some(summary.into());
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_conventional_names() {
        assert!(ToolName::new("read_file").is_ok());
        assert!(ToolName::new("search-files").is_ok());
    }

    #[test]
    fn rejects_illegal_names() {
        assert!(ToolName::new("").is_err());
        assert!(ToolName::new("read file").is_err());
        assert!(ToolName::new("read/file").is_err());
        assert!(ToolName::new("a".repeat(65)).is_err());
    }
}
