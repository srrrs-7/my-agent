//! The conversation aggregate.
//!
//! Owns one rule that is easy to get wrong and expensive when it breaks:
//! trimming history must never orphan a `tool_result` from the assistant turn
//! that requested it, or providers reject the next request outright.

use serde::{Deserialize, Serialize};

use super::message::{Message, Role};

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Conversation {
    messages: Vec<Message>,
}

impl Conversation {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_messages(messages: Vec<Message>) -> Self {
        Self { messages }
    }

    pub fn push(&mut self, message: Message) {
        self.messages.push(message);
    }

    pub fn messages(&self) -> &[Message] {
        &self.messages
    }

    pub fn into_messages(self) -> Vec<Message> {
        self.messages
    }

    pub fn len(&self) -> usize {
        self.messages.len()
    }

    pub fn is_empty(&self) -> bool {
        self.messages.is_empty()
    }

    pub fn approx_bytes(&self) -> usize {
        self.messages.iter().map(Message::approx_bytes).sum()
    }

    pub fn last_assistant_text(&self) -> Option<String> {
        self.messages
            .iter()
            .rev()
            .find(|message| message.role == Role::Assistant)
            .map(Message::text)
            .filter(|text| !text.trim().is_empty())
    }

    /// Drops the oldest turns until the history fits `max_bytes`.
    ///
    /// * `keep_recent` messages are never dropped, so the model always sees the
    ///   tail of the conversation even when a single turn blows the budget.
    /// * Any leading `tool` message left behind is dropped too: its matching
    ///   assistant `tool_call` is already gone and providers reject the pair.
    ///
    /// Returns how many messages were removed.
    pub fn trim_to_budget(&mut self, max_bytes: usize, keep_recent: usize) -> usize {
        // Decide how much of the prefix to drop before touching the vector:
        // repeatedly removing the front element while recomputing the total
        // would be quadratic in the length of the history.
        let mut total: usize = self.messages.iter().map(Message::approx_bytes).sum();
        let droppable = self.messages.len().saturating_sub(keep_recent);
        let mut drop_count = 0;

        while total > max_bytes && drop_count < droppable {
            total -= self.messages[drop_count].approx_bytes();
            drop_count += 1;
        }

        // Whatever is left must not start with a tool result: its matching
        // assistant `tool_call` is gone and providers reject the pair.
        while drop_count < self.messages.len() && self.messages[drop_count].role == Role::Tool {
            drop_count += 1;
        }

        self.messages.drain(..drop_count);
        drop_count
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::message::ContentBlock;
    use crate::model::tool::{ToolCall, ToolCallId, ToolName, ToolResult};
    use serde_json::json;

    fn call() -> ToolCall {
        ToolCall::new(
            ToolCallId::new("c1"),
            ToolName::new("read_file").unwrap(),
            json!({}),
        )
    }

    #[test]
    fn keeps_recent_messages_even_when_over_budget() {
        let mut conversation = Conversation::from_messages(vec![
            Message::user("a".repeat(500)),
            Message::assistant_text("b".repeat(500)),
            Message::user("c".repeat(500)),
        ]);
        conversation.trim_to_budget(10, 2);
        assert_eq!(conversation.len(), 2);
    }

    #[test]
    fn never_leaves_an_orphaned_tool_result_at_the_head() {
        let call = call();
        let mut conversation = Conversation::from_messages(vec![
            Message::user("x".repeat(1_000)),
            Message::assistant(vec![ContentBlock::ToolCall(call.clone())]),
            Message::tool_results(vec![ToolResult::ok(&call, "y".repeat(1_000))]),
            Message::assistant_text("done"),
        ]);
        conversation.trim_to_budget(64, 1);
        assert!(
            conversation.messages().first().map(|m| m.role) != Some(Role::Tool),
            "history must not start with a tool result"
        );
    }

    #[test]
    fn no_trimming_when_within_budget() {
        let mut conversation = Conversation::from_messages(vec![Message::user("hi")]);
        assert_eq!(conversation.trim_to_budget(10_000, 1), 0);
        assert_eq!(conversation.len(), 1);
    }
}
