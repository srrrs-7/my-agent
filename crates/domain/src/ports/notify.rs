//! Telling a human who is not watching the terminal that something happened.
//!
//! Distinct from [`EventSink`](super::events::EventSink), which narrates every
//! step to a terminal someone is looking at. A notification is for the person
//! who walked away: it fires on the few moments that need them - a prompt is
//! waiting for an answer, a turn finished, a turn gave up - and says only
//! enough to decide whether to come back.
//!
//! What goes into a [`Notification`] leaves the machine, so the vocabulary is
//! deliberately narrow. [`NotificationEvent::ApprovalPending`] carries the
//! tool's name and safety class but not the request's summary: that summary
//! quotes the model's arguments (file contents, commands, URLs), which are the
//! last thing that should be posted to a chat channel without the operator
//! reading them first.
//!
//! The types carry no `Serialize`. The wire format belongs to whichever
//! adapter speaks to a particular service, and a derive here would make the
//! shape of these structs a public protocol by accident.

use std::time::Duration;

use async_trait::async_trait;

use crate::error::NotifyError;
use crate::model::tool::{ToolName, ToolSafety};
use crate::ports::events::FinishReason;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notification {
    pub session_id: String,
    pub event: NotificationEvent,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NotificationEvent {
    /// A confirmation prompt is on screen and the loop is blocked on it.
    ApprovalPending { tool: ToolName, safety: ToolSafety },
    /// The loop returned an outcome. Whether that outcome counts as success is
    /// decided by [`Notification::kind`], not by the variant.
    TurnFinished {
        reason: FinishReason,
        /// Measured from the last moment a human interacted with the session,
        /// not from process start: a turn that waited an hour on a prompt
        /// nobody answered is not "an hour of work".
        elapsed: Duration,
        /// The opening of the answer, already shortened. `None` when the
        /// operator asked for no excerpt or the answer was empty.
        excerpt: Option<String>,
    },
    /// The loop returned an error - the provider itself broke.
    TurnFailed {
        elapsed: Duration,
        /// Coarse and human-readable. Never a raw error chain, which can carry
        /// endpoint URLs and response bodies.
        reason: String,
    },
}

/// The three things an operator can subscribe to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum NotificationKind {
    Approval,
    Completed,
    Failed,
}

impl NotificationKind {
    pub const ALL: [Self; 3] = [Self::Approval, Self::Completed, Self::Failed];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Approval => "approval",
            Self::Completed => "completed",
            Self::Failed => "failed",
        }
    }
}

impl Notification {
    /// Which subscription this notification belongs to.
    ///
    /// Mirrors `AgentOutcome::is_complete`: only a final answer is a
    /// completion. Running out of iterations or being cut off by the provider
    /// leaves the task unfinished, and someone waiting for "done" needs to
    /// hear that it is not.
    pub fn kind(&self) -> NotificationKind {
        match &self.event {
            NotificationEvent::ApprovalPending { .. } => NotificationKind::Approval,
            NotificationEvent::TurnFinished {
                reason: FinishReason::Completed,
                ..
            } => NotificationKind::Completed,
            NotificationEvent::TurnFinished { .. } | NotificationEvent::TurnFailed { .. } => {
                NotificationKind::Failed
            }
        }
    }
}

#[async_trait]
pub trait Notifier: Send + Sync {
    /// Delivers one notification.
    ///
    /// `Err` is for the caller to log and forget. Nothing about the agent's
    /// work may depend on whether a phone buzzed.
    async fn notify(&self, notification: &Notification) -> Result<(), NotifyError>;
}

/// Sends nothing.
///
/// The off switch: with no webhook configured this is what gets wired, so no
/// code above the composition root has a "notifications disabled" branch.
#[derive(Debug, Clone, Copy, Default)]
pub struct NullNotifier;

#[async_trait]
impl Notifier for NullNotifier {
    async fn notify(&self, _notification: &Notification) -> Result<(), NotifyError> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn finished(reason: FinishReason) -> Notification {
        Notification {
            session_id: "s".into(),
            event: NotificationEvent::TurnFinished {
                reason,
                elapsed: Duration::from_secs(1),
                excerpt: None,
            },
        }
    }

    #[test]
    fn only_a_final_answer_counts_as_completed() {
        assert_eq!(
            finished(FinishReason::Completed).kind(),
            NotificationKind::Completed
        );
        assert_eq!(
            finished(FinishReason::MaxIterations { limit: 25 }).kind(),
            NotificationKind::Failed,
            "running out of iterations leaves the task unfinished"
        );
        assert_eq!(
            finished(FinishReason::Stopped {
                stop_reason: "max_tokens".into()
            })
            .kind(),
            NotificationKind::Failed
        );
    }

    #[test]
    fn a_provider_failure_and_a_prompt_have_their_own_kinds() {
        let failed = Notification {
            session_id: "s".into(),
            event: NotificationEvent::TurnFailed {
                elapsed: Duration::ZERO,
                reason: "x".into(),
            },
        };
        assert_eq!(failed.kind(), NotificationKind::Failed);

        let pending = Notification {
            session_id: "s".into(),
            event: NotificationEvent::ApprovalPending {
                tool: ToolName::new("write_file").unwrap(),
                safety: ToolSafety::Mutating,
            },
        };
        assert_eq!(pending.kind(), NotificationKind::Approval);
    }
}
