//! Human-in-the-loop port.
//!
//! Every tool call passes through this gate before it runs. Making it a port
//! (rather than an `if interactive {}` inside the loop) is what lets the same
//! loop run interactively in a terminal, unattended in CI, and under a policy
//! engine later on.

use async_trait::async_trait;

use crate::error::ApprovalError;
use crate::model::tool::{ToolCall, ToolSafety};

#[derive(Debug, Clone, PartialEq)]
pub struct ApprovalRequest {
    pub call: ToolCall,
    pub safety: ToolSafety,
    /// One-line human description of what is about to happen.
    pub summary: String,
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
}
