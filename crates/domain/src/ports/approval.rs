//! Human-in-the-loop port.
//!
//! Every tool call passes through this gate before it runs. Making it a port
//! (rather than an `if interactive {}` inside the loop) is what lets the same
//! loop run interactively in a terminal, unattended in CI, and under a policy
//! engine later on.

use async_trait::async_trait;

use crate::error::ApprovalError;
use crate::model::dependency::ManifestChange;
use crate::model::tool::{PlannedWrite, ToolCall, ToolSafety};

#[derive(Debug, Clone, PartialEq)]
pub struct ApprovalRequest {
    pub call: ToolCall,
    pub safety: ToolSafety,
    /// Human description of what is about to happen, built from the model's
    /// arguments and **not sanitised**: it may contain control and
    /// bidirectional characters. A gate that shows it to a human passes it
    /// through [`crate::text::printable`] first - there, at the point of
    /// display, so that it happens once and cannot be forgotten upstream.
    pub summary: String,
    /// A human must answer this one: no approval policy (`auto`, read-only
    /// auto) and no earlier "approve everything" may answer it for them. Set
    /// by a gate further out - an operator's `ask` rule - and honoured by the
    /// gate that talks to the human. The dispatcher always starts it `false`.
    pub must_ask: bool,
}

/// A call that is about to change one or more dependency files.
///
/// Asked separately from [`ApprovalRequest`], and later: right before the call
/// runs, against the files as they are at that moment, so an earlier call in
/// the same turn cannot make the shown change stale.
#[derive(Debug, Clone, PartialEq)]
pub struct DependencyApprovalRequest {
    pub call: ToolCall,
    pub safety: ToolSafety,
    /// Never empty.
    pub changes: Vec<ManifestChange>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApprovalDecision {
    Approve,
    /// The reason is fed back to the model so it can adapt instead of retrying.
    Deny {
        reason: String,
    },
}

impl ApprovalDecision {
    pub fn is_approved(&self) -> bool {
        matches!(self, Self::Approve)
    }
}

#[async_trait]
pub trait ApprovalGate: Send + Sync {
    async fn authorize(&self, request: &ApprovalRequest)
    -> Result<ApprovalDecision, ApprovalError>;

    /// Whether a call approved by [`Self::authorize`] may still go ahead,
    /// now that `writes` - worked out right before it runs, after every
    /// earlier call in the turn - say which files it really reaches.
    ///
    /// The first approval is given before any call of the turn has run, so
    /// what it saw can be stale: an earlier `run_command` may have turned a
    /// path into a link to a file a rule protects. A gate whose answer
    /// depends on where a write lands (the operator's path rules) looks
    /// again here; `request` is the same one [`Self::authorize`] was given.
    /// Asked only for calls that plan writes.
    ///
    /// A gate that decided by the call alone has nothing to look at again
    /// and approves. A gate that wraps another **must forward this to it**:
    /// a decorator that answered itself would quietly stop the gates inside
    /// from ever looking again. There is deliberately no default, as with
    /// [`crate::ports::tool::Tool::preview`] - a forgotten forward is a
    /// compile error rather than a weaker gate.
    async fn reauthorize_write(
        &self,
        request: &ApprovalRequest,
        writes: &[PlannedWrite],
    ) -> Result<ApprovalDecision, ApprovalError>;

    /// Whether a change to the dependency files may go ahead.
    ///
    /// Separate from [`Self::authorize`] because no answer given there covers
    /// it - not an `auto` policy and not an "approve everything" - so a gate
    /// cannot satisfy it by accident.
    ///
    /// The default refuses. A gate written before this question existed, or
    /// one that never thought about it, must not turn into a gate that lets
    /// every dependency through.
    async fn authorize_dependency_change(
        &self,
        request: &DependencyApprovalRequest,
    ) -> Result<ApprovalDecision, ApprovalError> {
        let _ = request;
        Ok(ApprovalDecision::Deny {
            reason: "changes to dependency files need a human's approval, and nobody was asked"
                .to_string(),
        })
    }
}
