//! Who says yes to a tool call.
//!
//! The approval port is re-exported as it is in the domain: implementing
//! [`ApprovalGate`] is how an embedder puts the question to its own user (a
//! dialog, a chat message, a policy service). [`from_fn`] covers the common
//! case of a synchronous yes or no.
//!
//! Whatever gate the embedder gives, the agent stacks the same layers around
//! it as the binary does: plan mode, the operator's hooks (`AGENT_HOOKS`) and
//! permission rules (`AGENT_PERMISSIONS`) are consulted first, and
//! `AGENT_APPROVAL` answers in advance what it answers in the binary - `auto`
//! every call, `read-only` (the default) every read-only call. The gate is
//! asked the rest. Without a gate, the rest is denied, as in a session with
//! no terminal.
//!
//! A denial is not an error. Its reason goes back to the model as the call's
//! result, and the turn carries on.

use std::sync::Arc;

use agent_infrastructure::config::ApprovalPolicy;
use async_trait::async_trait;

pub use agent_domain::error::ApprovalError;
pub use agent_domain::model::tool::{PlannedWrite, ToolSafety};
pub use agent_domain::ports::approval::{
    ApprovalDecision, ApprovalGate, ApprovalRequest, DependencyApprovalRequest,
};

/// A gate that answers every call with `decide`.
///
/// It decides by the call alone, so a write it approved is not asked about
/// again once the files it really reaches are known. Changes to dependency
/// files are refused unless `AGENT_ALLOW_DEPENDENCY_CHANGES` is set:
/// answering them takes a gate that implements
/// [`ApprovalGate::authorize_dependency_change`].
pub fn from_fn<F>(decide: F) -> Arc<dyn ApprovalGate>
where
    F: Fn(&ApprovalRequest) -> ApprovalDecision + Send + Sync + 'static,
{
    Arc::new(FnGate(decide))
}

struct FnGate<F>(F);

#[async_trait]
impl<F> ApprovalGate for FnGate<F>
where
    F: Fn(&ApprovalRequest) -> ApprovalDecision + Send + Sync + 'static,
{
    async fn authorize(
        &self,
        request: &ApprovalRequest,
    ) -> Result<ApprovalDecision, ApprovalError> {
        Ok((self.0)(request))
    }

    async fn reauthorize_write(
        &self,
        _request: &ApprovalRequest,
        _writes: &[PlannedWrite],
    ) -> Result<ApprovalDecision, ApprovalError> {
        Ok(ApprovalDecision::Approve)
    }
}

/// The innermost gate of an embedded agent: the operator's policy first,
/// then the embedder's gate, if any.
pub(crate) struct PolicyGate {
    policy: ApprovalPolicy,
    allow_dependency_changes: bool,
    inner: Option<Arc<dyn ApprovalGate>>,
}

impl PolicyGate {
    pub(crate) fn new(
        policy: ApprovalPolicy,
        allow_dependency_changes: bool,
        inner: Option<Arc<dyn ApprovalGate>>,
    ) -> Self {
        Self {
            policy,
            allow_dependency_changes,
            inner,
        }
    }

    fn answered_in_advance(&self, request: &ApprovalRequest) -> bool {
        // An operator's `ask` rule outranks the policy: the policy is an
        // answer about calls in general, the rule is about this one.
        !request.must_ask
            && match self.policy {
                ApprovalPolicy::Auto => true,
                ApprovalPolicy::ReadOnlyAuto => request.safety.is_read_only(),
                ApprovalPolicy::Ask => false,
            }
    }
}

#[async_trait]
impl ApprovalGate for PolicyGate {
    async fn authorize(
        &self,
        request: &ApprovalRequest,
    ) -> Result<ApprovalDecision, ApprovalError> {
        if self.answered_in_advance(request) {
            return Ok(ApprovalDecision::Approve);
        }
        match &self.inner {
            Some(inner) => inner.authorize(request).await,
            None => Ok(ApprovalDecision::Deny {
                reason: format!(
                    "nobody is available to confirm the {} tool `{}`, so it was not run. Use a \
                     read-only tool, or tell the user what you would have done",
                    request.safety.label(),
                    request.call.name
                ),
            }),
        }
    }

    async fn reauthorize_write(
        &self,
        request: &ApprovalRequest,
        writes: &[PlannedWrite],
    ) -> Result<ApprovalDecision, ApprovalError> {
        match &self.inner {
            Some(inner) => inner.reauthorize_write(request, writes).await,
            None => Ok(ApprovalDecision::Approve),
        }
    }

    async fn authorize_dependency_change(
        &self,
        request: &DependencyApprovalRequest,
    ) -> Result<ApprovalDecision, ApprovalError> {
        // Deliberately not the policy: `auto` approves tool calls, and a
        // dependency change is a question no policy answers by itself.
        if self.allow_dependency_changes {
            return Ok(ApprovalDecision::Approve);
        }
        match &self.inner {
            Some(inner) => inner.authorize_dependency_change(request).await,
            None => Ok(ApprovalDecision::Deny {
                reason: "nobody is available to review a change to the project's dependencies, \
                         so it was refused"
                    .to_string(),
            }),
        }
    }
}
