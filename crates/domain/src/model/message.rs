//! Conversation messages.
//!
//! The representation is deliberately *block based* (like Anthropic's Messages
//! API) rather than flat strings: it is the richer of the two mainstream
//! shapes, so mapping down to OpenAI's `tool_calls` / `role: "tool"` form is
//! lossless, while the reverse would not be.

use serde::{Deserialize, Serialize};

use super::tool::{ToolCall, ToolResult};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    System,
    User,
    Assistant,
    /// Carries [`ContentBlock::ToolResult`] blocks back to the model.
    Tool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text { text: String },
    ToolCall(ToolCall),
    ToolResult(ToolResult),
}

impl ContentBlock {
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text { text: text.into() }
    }

    pub fn approx_bytes(&self) -> usize {
        match self {
            Self::Text { text } => text.len(),
            Self::ToolCall(call) => call.name.as_str().len() + call.arguments.to_string().len(),
            Self::ToolResult(result) => result.content.len(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: Vec<ContentBlock>,
}

impl Message {
    pub fn new(role: Role, content: Vec<ContentBlock>) -> Self {
        Self { role, content }
    }

    pub fn user(text: impl Into<String>) -> Self {
        Self::new(Role::User, vec![ContentBlock::text(text)])
    }

    pub fn assistant_text(text: impl Into<String>) -> Self {
        Self::new(Role::Assistant, vec![ContentBlock::text(text)])
    }

    pub fn assistant(content: Vec<ContentBlock>) -> Self {
        Self::new(Role::Assistant, content)
    }

    pub fn tool_results(results: Vec<ToolResult>) -> Self {
        Self::new(
            Role::Tool,
            results.into_iter().map(ContentBlock::ToolResult).collect(),
        )
    }

    /// All text blocks concatenated - what a human would read.
    pub fn text(&self) -> String {
        let parts: Vec<&str> = self
            .content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        parts.join("\n")
    }

    pub fn tool_calls(&self) -> impl Iterator<Item = &ToolCall> {
        self.content.iter().filter_map(|block| match block {
            ContentBlock::ToolCall(call) => Some(call),
            _ => None,
        })
    }

    pub fn has_tool_calls(&self) -> bool {
        self.tool_calls().next().is_some()
    }

    /// Cheap size proxy used by the context window policy. Not a token count -
    /// tokenisation belongs to a provider, the loop only needs a stable
    /// monotonic measure to decide what to drop first.
    pub fn approx_bytes(&self) -> usize {
        self.content
            .iter()
            .map(ContentBlock::approx_bytes)
            .sum::<usize>()
            + 16
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::tool::{ToolCallId, ToolName};
    use serde_json::json;

    #[test]
    fn text_concatenates_only_text_blocks() {
        let name = ToolName::new("read_file").unwrap();
        let message = Message::assistant(vec![
            ContentBlock::text("hello"),
            ContentBlock::ToolCall(ToolCall::new(ToolCallId::new("1"), name, json!({}))),
            ContentBlock::text("world"),
        ]);
        assert_eq!(message.text(), "hello\nworld");
        assert!(message.has_tool_calls());
    }
}
