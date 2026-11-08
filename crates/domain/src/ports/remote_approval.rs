//! Answering a confirmation prompt from somewhere other than the terminal.
//!
//! The terminal stays the authority: a remote answer only ever races it, and
//! whichever comes first is taken. That is why [`RemoteApprover::ask`] has no
//! way to say "no answer" - a remote channel that cannot answer simply never
//! resolves (or fails, and the caller goes on waiting at the terminal). A
//! failure is never a denial: a phone that lost its connection must not turn
//! into a "no" the human did not give.
//!
//! What crosses this port is deliberately narrower than an
//! [`ApprovalRequest`](super::approval::ApprovalRequest): the caller decides
//! how much of the model's arguments may leave the machine, hides known
//! secrets and cuts it to size before it gets here. Correlation tokens and
//! wire formats stay inside the adapter.
//!
//! Only the two plain answers exist. There is no "approve everything from now
//! on" and no free-text reason: the first would let one tap on a phone
//! disable every later prompt, and the second would hand whoever can post a
//! reply a way to put words in front of the model.

use async_trait::async_trait;

use crate::error::RemoteApprovalError;
use crate::model::tool::{ToolName, ToolSafety};

/// One prompt, as it may be shown away from the terminal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteApprovalRequest {
    pub session_id: String,
    pub tool: ToolName,
    pub safety: ToolSafety,
    /// What the call will do, already stripped of known secrets and cut to
    /// a size fit for a notification. Still model-written text.
    pub detail: String,
}

/// A prompt that can only be answered at the terminal, announced remotely so
/// the human knows to go back to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalOnlyNotice {
    pub session_id: String,
    pub tool: ToolName,
    pub safety: ToolSafety,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteAnswer {
    Approve,
    Deny,
}

#[async_trait]
pub trait RemoteApprover: Send + Sync {
    /// Whether [`Self::ask`] can answer at all.
    ///
    /// When it can, `ask` delivers the "waiting for approval" notification
    /// itself - with the buttons - and the caller must not send its own: one
    /// prompt, one buzz.
    fn is_active(&self) -> bool;

    /// Shows `request` remotely and waits for a valid answer to it.
    ///
    /// Resolves only with an answer to *this* request. Replies that are
    /// malformed, meant for another request, already used or too late are
    /// the adapter's to ignore. `Err` means the channel gave up on this
    /// prompt; the caller keeps waiting at the terminal.
    ///
    /// Dropping the future withdraws the question: whatever made a reply
    /// valid is discarded with it.
    async fn ask(
        &self,
        request: &RemoteApprovalRequest,
    ) -> Result<RemoteAnswer, RemoteApprovalError>;

    /// Tells the human a prompt is waiting that can only be answered at the
    /// terminal. Carries no way to answer.
    async fn announce_terminal_only(
        &self,
        notice: &TerminalOnlyNotice,
    ) -> Result<(), RemoteApprovalError>;
}

/// Answers nothing, ever.
///
/// The off switch: with no remote channel configured this is what gets
/// wired, and the terminal is the only thing that can answer.
#[derive(Debug, Clone, Copy, Default)]
pub struct NullRemoteApprover;

#[async_trait]
impl RemoteApprover for NullRemoteApprover {
    fn is_active(&self) -> bool {
        false
    }

    async fn ask(
        &self,
        _request: &RemoteApprovalRequest,
    ) -> Result<RemoteAnswer, RemoteApprovalError> {
        // Not an error: an error would be reported as a broken channel on
        // every prompt. Racing a future that never resolves is the same as
        // not racing at all.
        std::future::pending().await
    }

    async fn announce_terminal_only(
        &self,
        _notice: &TerminalOnlyNotice,
    ) -> Result<(), RemoteApprovalError> {
        Ok(())
    }
}
