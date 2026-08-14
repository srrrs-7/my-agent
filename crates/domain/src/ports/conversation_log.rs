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
}
