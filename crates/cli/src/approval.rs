//! Interactive approval gate.
//!
//! Denials are *not* failures: the reason is handed back to the model as a tool
//! result so it can pick another approach. That keeps a "no" cheap for the
//! human and informative for the agent.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use agent_domain::error::ApprovalError;
use agent_domain::ports::approval::{ApprovalDecision, ApprovalGate, ApprovalRequest};
use agent_infrastructure::config::ApprovalPolicy;
use async_trait::async_trait;

use crate::input::prompt_line;
use crate::notify::Notifications;

pub struct CliApprovalGate {
    policy: ApprovalPolicy,
    interactive: bool,
    /// Set once the user answers "a" - approve everything for the rest of the
    /// session.
    approve_all: AtomicBool,
    /// Told when a prompt is about to block on a human, and when the human
    /// answers. Shared with the subcommands, which time turns against it.
    notifications: Arc<Notifications>,
}

impl CliApprovalGate {
    pub fn new(
        policy: ApprovalPolicy,
        interactive: bool,
        notifications: Arc<Notifications>,
    ) -> Self {
        Self {
            policy,
            interactive,
            approve_all: AtomicBool::new(false),
            notifications,
        }
    }

    fn needs_confirmation(&self, request: &ApprovalRequest) -> bool {
        match self.policy {
            ApprovalPolicy::Auto => false,
            ApprovalPolicy::ReadOnlyAuto => !request.safety.is_read_only(),
            ApprovalPolicy::Ask => true,
        }
    }
}

#[async_trait]
impl ApprovalGate for CliApprovalGate {
    async fn authorize(
        &self,
        request: &ApprovalRequest,
    ) -> Result<ApprovalDecision, ApprovalError> {
        if !self.needs_confirmation(request) || self.approve_all.load(Ordering::SeqCst) {
            return Ok(ApprovalDecision::Approve);
        }

        if !self.interactive {
            return Ok(ApprovalDecision::Deny {
                reason: format!(
                    "this session is not interactive, so the {} tool `{}` could not be confirmed \
                     (re-run with --yes or AGENT_APPROVAL=auto to allow it)",
                    request.safety.label(),
                    request.call.name
                ),
            });
        }

        // `Y` is capitalised because a bare Enter approves - the conventional
        // shell notation for "this is the default".
        let question = format!(
            "\n  {} {}\n  allow? [Y]es / [n]o / [a]ll / or type a reason: ",
            request.safety.label(),
            request.summary
        );

        // Only here, once nothing is left that could answer for the human:
        // an auto-approved or auto-denied call blocks nobody. Fire and forget
        // - a delivery failure must not turn into a denied tool call.
        self.notifications.approval_pending(request);

        let answer = prompt_line(&question)
            .await
            .map_err(|error| ApprovalError::Unavailable(error.to_string()))?;
        self.notifications.engaged();

        match answer
            .map(|line| line.trim().to_ascii_lowercase())
            .as_deref()
        {
            Some("y") | Some("yes") | Some("") => Ok(ApprovalDecision::Approve),
            Some("a") | Some("all") => {
                self.approve_all.store(true, Ordering::SeqCst);
                Ok(ApprovalDecision::Approve)
            }
            // EOF (Ctrl-D) is a refusal, not an approval.
            None => Ok(ApprovalDecision::Deny {
                reason: "input stream closed".to_string(),
            }),
            Some(other) if other == "n" || other == "no" => Ok(ApprovalDecision::Deny {
                reason: "the user answered no".to_string(),
            }),
            // Anything else is treated as free-form feedback and forwarded.
            Some(other) => Ok(ApprovalDecision::Deny {
                reason: other.to_string(),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_domain::model::tool::{ToolCall, ToolCallId, ToolName, ToolSafety};
    use serde_json::json;

    use crate::notify::tests::{RecordingNotifier, everything};
    use crate::notify::{DRAIN_TIMEOUT, Notifications};

    fn gate(policy: ApprovalPolicy, interactive: bool) -> CliApprovalGate {
        CliApprovalGate::new(policy, interactive, Arc::new(Notifications::disabled()))
    }

    fn request(safety: ToolSafety) -> ApprovalRequest {
        ApprovalRequest {
            call: ToolCall::new(
                ToolCallId::new("1"),
                ToolName::new("write_file").unwrap(),
                json!({}),
            ),
            safety,
            summary: "write_file(path=\"a.rs\")".into(),
        }
    }

    #[test]
    fn auto_policy_never_asks() {
        let gate = gate(ApprovalPolicy::Auto, true);
        assert!(!gate.needs_confirmation(&request(ToolSafety::Destructive)));
    }

    #[test]
    fn read_only_policy_asks_only_for_writes() {
        let gate = gate(ApprovalPolicy::ReadOnlyAuto, true);
        assert!(!gate.needs_confirmation(&request(ToolSafety::ReadOnly)));
        assert!(gate.needs_confirmation(&request(ToolSafety::Mutating)));
    }

    #[test]
    fn ask_policy_asks_for_everything() {
        let gate = gate(ApprovalPolicy::Ask, true);
        assert!(gate.needs_confirmation(&request(ToolSafety::ReadOnly)));
    }

    #[tokio::test]
    async fn non_interactive_sessions_deny_instead_of_hanging() {
        let gate = gate(ApprovalPolicy::ReadOnlyAuto, false);
        let decision = gate
            .authorize(&request(ToolSafety::Mutating))
            .await
            .unwrap();
        match decision {
            ApprovalDecision::Deny { reason } => assert!(reason.contains("--yes")),
            other => panic!("expected a denial, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn no_one_is_paged_when_no_prompt_is_shown() {
        // Auto-approval and the non-interactive denial both decide without a
        // human, so nobody is waiting and nothing should buzz.
        let notifier = Arc::new(RecordingNotifier::default());
        let notifications = Arc::new(Notifications::new(notifier.clone(), everything()));

        for (policy, interactive) in [
            (ApprovalPolicy::Auto, true),
            (ApprovalPolicy::ReadOnlyAuto, false),
        ] {
            let gate = CliApprovalGate::new(policy, interactive, notifications.clone());
            gate.authorize(&request(ToolSafety::Destructive))
                .await
                .unwrap();
        }
        notifications.drain(DRAIN_TIMEOUT).await;

        let sent = notifier.sent.lock().unwrap();
        assert!(sent.is_empty(), "{sent:?}");
    }
}
