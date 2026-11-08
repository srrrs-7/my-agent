//! `pre_tool_use`: the operator's hooks, asked before the rules and the
//! human.
//!
//! Between plan mode and the permission rules in the approval stack
//! (`PlanModeGate(HookGate(RuleGate(..)))`): plan mode refuses before a hook
//! is started for a call that could not run anyway, and a hook is consulted
//! before an `allow` rule could spare the human the question - so the rules
//! can skip the human, but never the hooks.

use std::sync::Arc;

use agent_domain::error::{ApprovalError, CommandError};
use agent_domain::ports::approval::{
    ApprovalDecision, ApprovalGate, ApprovalRequest, DependencyApprovalRequest,
};
use agent_domain::ports::command::CommandOutput;
use async_trait::async_trait;

use super::{Hooks, encode};

/// Refuses a call any matching pre hook refuses; asks `inner` about the
/// rest. Never approves on its own.
pub struct HookGate {
    inner: Arc<dyn ApprovalGate>,
    hooks: Arc<Hooks>,
}

impl HookGate {
    pub fn new(inner: Arc<dyn ApprovalGate>, hooks: Arc<Hooks>) -> Self {
        Self { inner, hooks }
    }

    /// What the model is told when a hook produced `result`, or `None` when
    /// it let the call go on.
    fn refusal(
        &self,
        timeout_secs: u64,
        result: Result<CommandOutput, CommandError>,
    ) -> Option<String> {
        let output = match result {
            Ok(output) if output.exit_code == Some(0) => return None,
            Ok(output) => output,
            Err(CommandError::TimedOut { .. }) => {
                return Some(format!(
                    "the operator's pre_tool_use hook did not finish within {timeout_secs}s, so \
                     the call was refused"
                ));
            }
            // Fails closed: a check that could not run has not passed.
            Err(error) => {
                return Some(format!(
                    "the operator's pre_tool_use hook could not run ({error}), so the call was \
                     refused"
                ));
            }
        };
        let said = self.hooks.said(&output.stderr);
        if !said.is_empty() {
            return Some(said);
        }
        Some(match output.exit_code {
            Some(code) => format!(
                "the operator's pre_tool_use hook refused this call (exit code {code}) without \
                 giving a reason"
            ),
            None => "the operator's pre_tool_use hook was killed by a signal, so the call was \
                     refused"
                .to_string(),
        })
    }
}

#[async_trait]
impl ApprovalGate for HookGate {
    async fn authorize(
        &self,
        request: &ApprovalRequest,
    ) -> Result<ApprovalDecision, ApprovalError> {
        let call = &request.call;
        let matching = self
            .hooks
            .config
            .pre_tool_use
            .iter()
            .filter(|hook| hook.matches(call.name.as_str()));
        for hook in matching {
            let payload = self.hooks.tool_payload(
                "pre_tool_use",
                &call.name,
                request.safety,
                Some(&call.id),
                &call.arguments,
            );
            let result = self
                .hooks
                .runner
                .run(&hook.command, encode(&payload), hook.timeout())
                .await;
            if let Some(reason) = self.refusal(hook.timeout_secs, result) {
                return Ok(ApprovalDecision::Deny { reason });
            }
        }

        let decision = self.inner.authorize(request).await?;
        if decision.is_approved() && self.hooks.observes(call.name.as_str()) {
            self.hooks.note(&call.name, &call.arguments, &call.id);
        }
        Ok(decision)
    }

    /// Straight through: no hook answers this, in either direction. A hook
    /// that exited 0 is not a human who saw the new dependency.
    async fn authorize_dependency_change(
        &self,
        request: &DependencyApprovalRequest,
    ) -> Result<ApprovalDecision, ApprovalError> {
        self.inner.authorize_dependency_change(request).await
    }
}
