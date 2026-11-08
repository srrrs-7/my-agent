//! Human-in-the-loop port.
//!
//! Every tool call passes through this gate before it runs. Making it a port
//! (rather than an `if interactive {}` inside the loop) is what lets the same
//! loop run interactively in a terminal, unattended in CI, and under a policy
//! engine later on.

use async_trait::async_trait;

use crate::error::ApprovalError;
use crate::model::dependency::ManifestChange;
use crate::model::tool::{ToolCall, ToolSafety};

#[derive(Debug, Clone, PartialEq)]
pub struct ApprovalRequest {
    pub call: ToolCall,
    pub safety: ToolSafety,
    /// One-line human description of what is about to happen.
    pub summary: String,
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
