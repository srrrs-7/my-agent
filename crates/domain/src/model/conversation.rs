//! The conversation aggregate.
//!
//! Owns one rule that is easy to get wrong and expensive when it breaks:
//! shortening the history must never orphan a `tool_result` from the assistant
//! turn that requested it, or providers reject the next request outright.
//!
//! There are two ways to shorten it, and the rule is the same for both.
//! [`Conversation::trim_to_budget`] drops the oldest turns outright;
//! [`Conversation::replace_prefix`] folds them into a single message that
//! someone else has summarised. Only the boundary logic lives here - deciding
//! *what* the summary says is a use case, not a domain rule.

use serde::{Deserialize, Serialize};

use super::message::{Message, Role};

/// Below this, folding a prefix into a summary is not worth a model
/// round-trip: one message replaced by one message saves nothing.
const MIN_FOLDED_MESSAGES: usize = 2;

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

    /// Where a compaction may cut, keeping `keep_recent` messages verbatim, or
    /// `None` when folding what is left would not be worth a model round-trip.
    ///
    /// The caller summarises `messages()[..split]` and hands the result to
    /// [`Self::replace_prefix`]. Nothing here mutates, so a summarisation that
    /// fails leaves the history exactly as it was and the caller can fall back
    /// to [`Self::trim_to_budget`].
    pub fn compaction_split(&self, keep_recent: usize) -> Option<usize> {
        let split = self.fold_boundary(self.messages.len().saturating_sub(keep_recent));
        (split >= MIN_FOLDED_MESSAGES).then_some(split)
    }

    /// Replaces everything before `split` with `summary`, returning how many
    /// messages were folded away.
    ///
    /// `split` is moved if it would leave the history malformed, by the same
    /// rules [`Self::compaction_split`] applies - so a split that came from
    /// there is used as given, and one that did not is still safe.
    pub fn replace_prefix(&mut self, split: usize, summary: Message) -> usize {
        let split = self.fold_boundary(split);
        if split == 0 {
            return 0;
        }
        self.messages.splice(..split, std::iter::once(summary));
        split
    }

    /// Nearest index to `requested` that cuts the history cleanly.
    ///
    /// Two rules, and they cannot be satisfied by moving the cut in one
    /// direction only:
    ///
    /// * The tail must not *begin* with a tool result, whose matching call
    ///   would be on the other side of the cut. Move forward until it does not.
    /// * The prefix must not *end* with a request for tools whose results
    ///   stayed behind. That would orphan them, and it would also make the
    ///   prefix invalid as a request in its own right - which matters because
    ///   the caller sends it to a model to be summarised. Move back until it
    ///   does not.
    ///
    /// Moving back cannot re-break the first rule: the message it lands on
    /// carries tool calls, so it is an assistant turn, not a tool result.
    ///
    /// In a well-formed history the two rules often *could* both resolve the
    /// same cut, since a tool result is always preceded by the call that asked
    /// for it. Forward runs first because it is the one that makes progress:
    /// a history ending in a tool result would otherwise retreat all the way to
    /// zero and fold nothing at all.
    fn fold_boundary(&self, requested: usize) -> usize {
        let mut split = requested.min(self.messages.len());

        while split < self.messages.len() && self.messages[split].role == Role::Tool {
            split += 1;
        }
        while split > 0 && self.messages[split - 1].has_tool_calls() {
            split -= 1;
        }

        split
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

    /// `[user, assistant(call), tool, assistant, user, assistant]`
    fn with_a_tool_turn() -> Conversation {
        let call = call();
        Conversation::from_messages(vec![
            Message::user("first"),
            Message::assistant(vec![ContentBlock::ToolCall(call.clone())]),
            Message::tool_results(vec![ToolResult::ok(&call, "contents")]),
            Message::assistant_text("read it"),
            Message::user("second"),
            Message::assistant_text("done"),
        ])
    }

    #[test]
    fn a_compaction_folds_everything_except_the_recent_tail() {
        let mut conversation = with_a_tool_turn();
        let split = conversation
            .compaction_split(2)
            .expect("there is history to fold");

        assert_eq!(split, 4, "the tail keeps the last two messages");
        assert_eq!(
            conversation.replace_prefix(split, Message::user("SUMMARY")),
            4
        );
        assert_eq!(conversation.len(), 3);
        assert_eq!(conversation.messages()[0].text(), "SUMMARY");
        assert_eq!(conversation.messages()[1].text(), "second");
    }

    /// The cut lands between an assistant's tool call and its result, which is
    /// the case that orphans a result if it is taken literally.
    #[test]
    fn a_cut_between_a_call_and_its_result_moves_past_the_result() {
        let mut conversation = with_a_tool_turn();
        // keep_recent = 4 asks to cut at index 2 - exactly the tool result.
        let split = conversation
            .compaction_split(4)
            .expect("there is history to fold");

        assert_eq!(
            split, 3,
            "the result is folded with the call that asked for it"
        );
        conversation.replace_prefix(split, Message::user("SUMMARY"));
        assert_ne!(
            conversation.messages()[1].role,
            Role::Tool,
            "history must not resume with an orphaned tool result"
        );
    }

    /// Reachable whenever a caller compacts after the model has asked for tools
    /// but before the results exist. Folding that assistant turn away would
    /// leave its results with nothing to attach to - and would also make the
    /// prefix invalid as a summarisation request on its own.
    #[test]
    fn a_trailing_tool_call_is_left_for_the_tail() {
        let call = call();
        let conversation = Conversation::from_messages(vec![
            Message::user("go"),
            Message::assistant_text("thinking"),
            Message::assistant(vec![ContentBlock::ToolCall(call)]),
        ]);

        assert_eq!(
            conversation.compaction_split(0),
            Some(2),
            "the unanswered call stays out of the fold"
        );
    }

    #[test]
    fn folding_a_single_message_is_not_worth_a_round_trip() {
        let conversation =
            Conversation::from_messages(vec![Message::user("a"), Message::assistant_text("b")]);
        assert_eq!(conversation.compaction_split(1), None);
        assert_eq!(conversation.compaction_split(9), None, "nothing to fold");
    }

    #[test]
    fn replacing_a_prefix_that_would_orphan_a_result_moves_the_cut() {
        // A split nobody planned: straight into the middle of a tool turn.
        let mut conversation = with_a_tool_turn();
        let folded = conversation.replace_prefix(2, Message::user("SUMMARY"));

        assert_eq!(folded, 3, "the cut moved past the tool result");
        assert_ne!(conversation.messages()[1].role, Role::Tool);
    }

    #[test]
    fn a_compacted_history_opens_with_the_summary() {
        // Providers expect a conversation to start with a user message, and
        // after a compaction the summary is that message.
        let mut conversation = with_a_tool_turn();
        let split = conversation.compaction_split(2).unwrap();
        conversation.replace_prefix(split, Message::user("SUMMARY"));

        assert_eq!(conversation.messages()[0].role, Role::User);
    }
}
