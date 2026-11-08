//! The durable record of what was said.
//!
//! Distinct from [`EventSink`](super::events::EventSink), which exists so the
//! loop can *narrate* itself to a terminal. Events are a summary written for a
//! reader: `AssistantMessage` carries the prose and not the tool calls beside
//! it, and tool results arrive already reduced to a one-line summary. That is
//! the right shape for a progress display and the wrong shape for a record,
//! because a record is judged by what it still has when someone comes back for
//! it.
//!
//! This port carries whole messages instead, and the loop writes to it before
//! anything is allowed to shorten the history - which is the point. The
//! conversation loses content twice over on its way through a long session:
//! compaction replaces old turns with a summary, and trimming deletes what
//! still does not fit. Both are correct, and both are irreversible. What is
//! written here is what survives them.

use async_trait::async_trait;

use crate::error::FsError;
use crate::model::message::Message;
use crate::model::tool::ToolCallId;

/// What joins a parent session's id to the call that started a sub-agent,
/// in the sub-agent's own session id.
///
/// The one place the naming rule is written down, because two layers depend
/// on it: the application names a sub-agent's session with
/// [`subagent_session_id`], and a store tells a sub-agent's record from a
/// session someone could resume with [`parent_session`]. A UUID - which is
/// what a session the CLI starts is called - never contains one.
pub const SUBAGENT_SEPARATOR: char = '.';

/// The session id of the sub-agent that the call `call_id` of the session
/// `parent` started: `"{parent}.{call_id}"`.
///
/// Derived rather than generated, so the record of a sub-agent can be found
/// from the parent's record alone - the call that started it is in there -
/// and this crate needs no source of randomness.
pub fn subagent_session_id(parent: &str, call_id: &ToolCallId) -> String {
    format!("{parent}{SUBAGENT_SEPARATOR}{call_id}")
}

/// `(parent, call id)` when `session_id` names a sub-agent's session, split
/// at the first [`SUBAGENT_SEPARATOR`]. `None` for any other session -
/// including an id that starts or ends with the separator, which no
/// sub-agent is ever given.
pub fn parent_session(session_id: &str) -> Option<(&str, &str)> {
    session_id
        .split_once(SUBAGENT_SEPARATOR)
        .filter(|(parent, call)| !parent.is_empty() && !call.is_empty())
}

#[async_trait]
pub trait ConversationLog: Send + Sync {
    /// Appends `messages` to the record of `session_id`, in order.
    ///
    /// Implementations must be append-only. Nothing that reaches here is ever
    /// revised: a message that turned out to be wrong is part of what happened.
    ///
    /// Returning `Err` must not be taken as a reason to abandon the turn - the
    /// caller warns and carries on, the same way it does when a compaction
    /// fails. A session that stops working because its logging broke is worse
    /// than one that is missing a line of its log.
    async fn append(&self, session_id: &str, messages: &[Message]) -> Result<(), FsError>;

    /// Records that the conversation of `session_id` was taken back to just
    /// before the message numbered `seq` (see
    /// [`Conversation::rewind_to`](crate::model::conversation::Conversation::rewind_to)).
    ///
    /// Appended like a message, never by editing what is already written: the
    /// messages taken back stay in the record, and a reader replaying it drops
    /// those numbered `seq` or later when it reaches the mark. Errors are
    /// treated as for [`Self::append`].
    ///
    /// No default: one that wrote nothing would bring back, on the next
    /// resume, every message the user asked to take back.
    async fn rewind(&self, session_id: &str, seq: u64) -> Result<(), FsError>;
}

/// Writes nothing.
///
/// The default, and the whole of the off switch: when logging is disabled
/// nothing downstream needs to know, because there is nothing to skip. Also
/// what the application tests run against, so that a suite of two hundred
/// tests does not write two hundred transcripts.
#[derive(Debug, Clone, Copy, Default)]
pub struct NullConversationLog;

#[async_trait]
impl ConversationLog for NullConversationLog {
    async fn append(&self, _session_id: &str, _messages: &[Message]) -> Result<(), FsError> {
        Ok(())
    }

    async fn rewind(&self, _session_id: &str, _seq: u64) -> Result<(), FsError> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_subagent_session_id_splits_back_into_its_parent_and_call() {
        let id = subagent_session_id("0f3c-uuid", &ToolCallId::new("call_7"));
        assert_eq!(id, "0f3c-uuid.call_7");
        assert_eq!(parent_session(&id), Some(("0f3c-uuid", "call_7")));
    }

    #[test]
    fn a_call_id_with_the_separator_in_it_still_names_the_right_parent() {
        let id = subagent_session_id("parent", &ToolCallId::new("odd.id"));
        assert_eq!(parent_session(&id), Some(("parent", "odd.id")));
    }

    #[test]
    fn an_ordinary_session_has_no_parent() {
        for id in ["0f3c-uuid", "", ".", "..", ".leading", "trailing."] {
            assert_eq!(parent_session(id), None, "{id:?}");
        }
    }
}
