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
//!
//! ## What survives is decided by order and size, never by the clock
//!
//! Every message carries the position it took in the session
//! ([`Message::seq`](super::message::Message::seq)), assigned here as it is
//! appended. That number is the only notion of "recent" this aggregate has,
//! and it is enough: what the retention policy needs to know is how many
//! messages ago something was said and how many bytes it costs to keep saying
//! it, neither of which a wall clock answers better. It also keeps the
//! aggregate pure - no clock to inject, and a policy that behaves identically
//! on a session replayed a year later.
//!
//! In a conversation that has only ever been appended to, that number says the
//! same thing as the vector index. It is kept as a field anyway because it is
//! the part that survives being written to disk and read back, and because it
//! keeps counting rather than restarting - so a session that is saved, resumed
//! and then folded still has one ordering everybody agrees on.

use serde::{Deserialize, Serialize};

use super::message::{Message, Role};

/// Below this, folding a prefix into a summary is not worth a model
/// round-trip: one message replaced by one message saves nothing.
const MIN_FOLDED_MESSAGES: usize = 2;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Conversation {
    messages: Vec<Message>,

    /// The number the next appended message gets.
    ///
    /// Counts messages for the life of the session and is deliberately *not*
    /// reset by a compaction: folding a prefix away shortens the vector, and
    /// without this the history would forget how deep it already is the moment
    /// it was shortened. Nothing outside reads it; what callers ask for is
    /// [`Self::distance_from_newest`].
    #[serde(default)]
    next_seq: u64,
}

impl Conversation {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_messages(messages: Vec<Message>) -> Self {
        let mut conversation = Self::default();
        for message in messages {
            conversation.push(message);
        }
        conversation
    }

    pub fn push(&mut self, message: Message) {
        self.messages.push(message.with_seq(self.next_seq));
        self.next_seq += 1;
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

    /// Drops a trailing assistant turn whose tool results never arrived,
    /// returning how many messages went.
    ///
    /// The counterpart of the rule [`Self::trim_to_budget`] enforces at the
    /// front. A history reconstructed from a record that stops mid-turn - a
    /// crash between the model asking for a tool and the result being written -
    /// ends with a `tool_call` nothing will ever answer, and the next request
    /// carries it to a provider that rejects the pair. Nobody is going to run
    /// that tool now, so the call goes.
    ///
    /// Loops rather than dropping one message because a turn may hold several
    /// unanswered assistant messages; stops at anything else, so a complete
    /// turn is never touched.
    pub fn drop_trailing_unanswered_calls(&mut self) -> usize {
        let mut end = self.messages.len();
        while end > 0 && self.messages[end - 1].has_tool_calls() {
            end -= 1;
        }

        let dropped = self.messages.len() - end;
        self.messages.truncate(end);
        dropped
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

    /// Where a compaction may cut when the tail is bounded by size as well as
    /// by message count.
    ///
    /// [`Self::compaction_split`] keeps a fixed number of recent messages
    /// whatever they weigh, and that is the wrong measure as soon as one of
    /// them is a 32 KB tool result: twelve of those are more than the entire
    /// history budget, so the tail that was supposed to be protected is
    /// precisely what forces the fresh summary to be deleted again a moment
    /// later. The number of messages worth keeping is not a constant - it
    /// depends on how big they turned out to be.
    ///
    /// So this walks back from the newest message, taking messages while both
    /// caps still allow: at most `keep_recent` of them, at most
    /// `keep_recent_bytes` in total. The newest message is taken whatever it
    /// weighs, because it is the turn about to be answered and a tail of
    /// nothing leaves the model with nothing to answer.
    ///
    /// It can only ever cut *later* than [`Self::compaction_split`] would,
    /// never earlier, and it leaves [`Self::trim_to_budget`] alone - so
    /// nothing here makes outright deletion more likely. A message this moves
    /// is a message that ends up in the summary instead of in the tail.
    pub fn compaction_split_within(
        &self,
        keep_recent: usize,
        keep_recent_bytes: usize,
    ) -> Option<usize> {
        let split = self.fold_boundary(self.verbatim_tail_start(keep_recent, keep_recent_bytes));
        (split >= MIN_FOLDED_MESSAGES).then_some(split)
    }

    /// How far the message at `index` sits behind the newest one, counted in
    /// messages.
    ///
    /// This is the weight the retention policy is built on: not how old a
    /// message is in seconds, but how many turns of conversation have happened
    /// since. A caller uses it to decide how much of a message is still worth
    /// carrying - see `HistoryCompactor` in the application layer.
    ///
    /// Read from the sequence numbers rather than from the vector so that the
    /// answer is the same before and after the history is persisted, and falls
    /// back to the vector position for a history written before the numbers
    /// existed.
    pub fn distance_from_newest(&self, index: usize) -> u64 {
        let newest = self.messages.len().saturating_sub(1);
        let seq_at = |at: usize| self.messages.get(at).and_then(|message| message.seq);

        match (seq_at(newest), seq_at(index)) {
            (Some(newest_seq), Some(seq)) => newest_seq.saturating_sub(seq),
            _ => newest.saturating_sub(index) as u64,
        }
    }

    /// First index of the newest run of messages that fits both caps.
    fn verbatim_tail_start(&self, keep_recent: usize, keep_recent_bytes: usize) -> usize {
        let mut used = 0usize;
        let mut start = self.messages.len();

        for (index, message) in self.messages.iter().enumerate().rev().take(keep_recent) {
            used += message.approx_bytes();
            if used > keep_recent_bytes && start < self.messages.len() {
                break;
            }
            start = index;
        }

        start
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
        // The summary inherits the number of the last message it swallowed,
        // which keeps the session counting monotonically across the fold.
        //
        // The *last* rather than the first on purpose. A summary is a record
        // written just now, not the oldest thing in the history, and numbering
        // it as the oldest would tell every later policy to discard first the
        // one message that was written to stop things being discarded.
        let seq = self.messages[split - 1].seq.unwrap_or((split - 1) as u64);
        self.messages
            .splice(..split, std::iter::once(summary.with_seq(seq)));
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

    /// 116 approximate bytes each, so a budget divides into a whole number of
    /// them and the arithmetic in these tests stays readable.
    fn sized(text_bytes: usize) -> Message {
        Message::assistant_text("x".repeat(text_bytes))
    }

    fn six_equal_messages() -> Conversation {
        Conversation::from_messages((0..6).map(|_| sized(100)).collect())
    }

    #[test]
    fn the_byte_cap_shortens_the_verbatim_tail() {
        let conversation = six_equal_messages();
        assert_eq!(
            conversation.compaction_split(12),
            None,
            "by message count alone all six are protected and nothing is folded"
        );

        // 250 bytes is room for two of them, so the other four are folded -
        // which is the whole point: how many messages are worth keeping
        // depends on how big they turned out to be.
        assert_eq!(conversation.compaction_split_within(12, 250), Some(4));
    }

    #[test]
    fn the_message_cap_still_applies_when_the_bytes_are_generous() {
        let conversation = six_equal_messages();
        assert_eq!(
            conversation.compaction_split_within(2, 1_000_000),
            conversation.compaction_split(2),
            "a budget nothing can exhaust must decide exactly what the count did"
        );
    }

    #[test]
    fn the_newest_message_is_kept_whatever_it_weighs() {
        // A 5 KB tool result lands with 250 bytes of room. Folding it away
        // would leave the model with nothing to answer.
        let conversation =
            Conversation::from_messages(vec![sized(100), sized(100), sized(100), sized(5_000)]);

        assert_eq!(conversation.compaction_split_within(12, 250), Some(3));
    }

    #[test]
    fn a_byte_driven_cut_still_never_orphans_a_tool_result() {
        let mut conversation = with_a_tool_turn();
        let split = conversation
            .compaction_split_within(12, 100)
            .expect("a budget this small must fold something");

        conversation.replace_prefix(split, Message::user("SUMMARY"));
        assert_eq!(conversation.messages()[0].role, Role::User);
        assert_ne!(
            conversation.messages()[1].role,
            Role::Tool,
            "history must not resume with a result whose call was folded away"
        );
    }

    #[test]
    fn sequence_numbers_survive_a_fold_and_keep_counting() {
        let mut conversation = with_a_tool_turn();
        assert_eq!(conversation.messages()[5].seq, Some(5));

        let split = conversation.compaction_split(2).expect("there is history");
        conversation.replace_prefix(split, Message::user("SUMMARY"));

        assert_eq!(
            conversation.messages()[0].seq,
            Some(3),
            "the summary takes the number of the last message it swallowed"
        );
        assert_eq!(
            conversation.messages()[2].seq,
            Some(5),
            "the tail keeps its own"
        );

        conversation.push(Message::user("third"));
        assert_eq!(
            conversation.messages()[3].seq,
            Some(6),
            "the counter must not restart just because the vector shrank"
        );
        assert_eq!(conversation.distance_from_newest(0), 3);
    }

    #[test]
    fn a_history_that_stops_mid_turn_loses_the_unanswered_call() {
        // What a record written up to the moment of a crash looks like.
        let call = call();
        let mut conversation = Conversation::from_messages(vec![
            Message::user("go"),
            Message::assistant_text("looking"),
            Message::assistant(vec![ContentBlock::ToolCall(call)]),
        ]);

        assert_eq!(conversation.drop_trailing_unanswered_calls(), 1);
        assert_eq!(conversation.len(), 2);
        assert_eq!(conversation.messages()[1].text(), "looking");
    }

    #[test]
    fn a_complete_turn_is_left_alone() {
        let mut conversation = with_a_tool_turn();
        let before = conversation.clone();

        assert_eq!(conversation.drop_trailing_unanswered_calls(), 0);
        assert_eq!(conversation, before);
    }

    #[test]
    fn a_history_without_sequence_numbers_still_measures_distance() {
        // What a session persisted before the numbers existed looks like.
        let conversation: Conversation = serde_json::from_str(
            r#"{"messages":[
                 {"role":"user","content":[{"type":"text","text":"a"}]},
                 {"role":"assistant","content":[{"type":"text","text":"b"}]}
               ]}"#,
        )
        .expect("an older history must still load");

        assert_eq!(conversation.distance_from_newest(0), 1);
        assert_eq!(conversation.distance_from_newest(1), 0);
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
