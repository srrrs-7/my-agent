//! Interactive approval gate.
//!
//! Denials are *not* failures: the reason is handed back to the model as a tool
//! result so it can pick another approach. That keeps a "no" cheap for the
//! human and informative for the agent.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use agent_domain::error::ApprovalError;
use agent_domain::ports::approval::{ApprovalDecision, ApprovalGate, ApprovalRequest};
use agent_infrastructure::config::ApprovalPolicy;
use async_trait::async_trait;

use crate::input::prompt_line;
use crate::notify::Notifications;

/// Shows a question and reads the answer; `None` means end of input.
type LineFuture = Pin<Box<dyn Future<Output = anyhow::Result<Option<String>>> + Send>>;
type ReadLine = Box<dyn Fn(String) -> LineFuture + Send + Sync>;

pub struct CliApprovalGate {
    policy: ApprovalPolicy,
    interactive: bool,
    /// Set once the user answers "a" - approve everything for the rest of the
    /// session.
    approve_all: AtomicBool,
    /// Told when a prompt is about to block on a human, and when the human
    /// answers. Shared with the subcommands, which time turns against it.
    notifications: Arc<Notifications>,
    /// The terminal in production. Replaceable in tests, because the
    /// prompting path - the only one that pages anyone - cannot otherwise be
    /// reached without a TTY.
    read_line: ReadLine,
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
            read_line: Box::new(|question| Box::pin(async move { prompt_line(&question).await })),
        }
    }

    /// Answers prompts with `read_line` instead of the terminal.
    #[cfg(test)]
    fn with_read_line(mut self, read_line: ReadLine) -> Self {
        self.read_line = read_line;
        self
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

        let answer = (self.read_line)(question)
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

    use std::sync::Mutex;
    use std::sync::atomic::AtomicUsize;
    use std::time::Instant;

    use agent_domain::ports::notify::{NotificationEvent, NotificationKind};

    use crate::notify::tests::{RecordingNotifier, everything};
    use crate::notify::{DRAIN_TIMEOUT, Notifications};

    /// A terminal whose human always types `answer`.
    fn answering(answer: Option<&'static str>) -> ReadLine {
        Box::new(move |_| Box::pin(async move { Ok(answer.map(String::from)) }))
    }

    /// An interactive gate that prompts for every call and reports to `notifier`.
    fn prompting(
        notifier: &Arc<RecordingNotifier>,
        read_line: ReadLine,
    ) -> (CliApprovalGate, Arc<Notifications>) {
        let notifications = Arc::new(Notifications::new(notifier.clone(), everything()));
        notifications.begin_turn("session-7");
        let gate = CliApprovalGate::new(ApprovalPolicy::Ask, true, notifications.clone())
            .with_read_line(read_line);
        (gate, notifications)
    }

    fn secret_request() -> ApprovalRequest {
        ApprovalRequest {
            call: ToolCall::new(
                ToolCallId::new("1"),
                ToolName::new("write_file").unwrap(),
                json!({"path": ".env", "content": "API_KEY=sk-live-123"}),
            ),
            safety: ToolSafety::Mutating,
            summary: "write_file(path=\".env\", content=\"API_KEY=sk-live-123\")".into(),
        }
    }

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

    #[tokio::test]
    async fn a_shown_prompt_pages_with_the_tool_name_but_none_of_its_arguments() {
        let notifier = Arc::new(RecordingNotifier::default());
        let (gate, notifications) = prompting(&notifier, answering(Some("y\n")));

        let decision = gate.authorize(&secret_request()).await.unwrap();
        notifications.drain(DRAIN_TIMEOUT).await;

        assert_eq!(decision, ApprovalDecision::Approve);
        let sent = notifier.sent.lock().unwrap();
        assert_eq!(sent.len(), 1, "one prompt, one page: {sent:?}");
        assert_eq!(sent[0].session_id, "session-7", "{sent:?}");
        assert_eq!(
            sent[0].event,
            NotificationEvent::ApprovalPending {
                tool: ToolName::new("write_file").unwrap(),
                safety: ToolSafety::Mutating,
            }
        );
        let shown = format!("{sent:?}");
        assert!(
            !shown.contains("sk-live") && !shown.contains(".env"),
            "the arguments must not leave the machine: {shown}"
        );
    }

    #[tokio::test]
    async fn the_page_goes_out_whatever_the_answer_turns_out_to_be() {
        // The human was needed the moment the prompt appeared; a later "no"
        // or Ctrl-D does not retract that.
        for answer in [None, Some("n\n"), Some("use a different file\n")] {
            let notifier = Arc::new(RecordingNotifier::default());
            let (gate, notifications) = prompting(&notifier, answering(answer));

            let decision = gate.authorize(&secret_request()).await.unwrap();
            notifications.drain(DRAIN_TIMEOUT).await;

            assert!(
                matches!(decision, ApprovalDecision::Deny { .. }),
                "{answer:?} -> {decision:?}"
            );
            let sent = notifier.sent.lock().unwrap();
            assert_eq!(sent.len(), 1, "{answer:?}: {sent:?}");
            assert_eq!(sent[0].kind(), NotificationKind::Approval, "{sent:?}");
        }
    }

    #[tokio::test]
    async fn answering_a_prompt_restarts_the_turn_clock() {
        // Time spent looking at the prompt is the human's, not the turn's:
        // the clock must restart no earlier than the moment of asking.
        let notifier = Arc::new(RecordingNotifier::default());
        let asked_at = Arc::new(Mutex::new(None));
        let notifications = Arc::new(Notifications::new(notifier.clone(), everything()));
        notifications.begin_turn("s");
        let turn_started = notifications.last_engaged();

        let record = asked_at.clone();
        let read_line: ReadLine = Box::new(move |_| {
            // Not a sleep: the monotonic clock only has to tick once, so the
            // moment of asking is strictly after the turn started.
            let mut now = Instant::now();
            while now <= turn_started {
                std::hint::spin_loop();
                now = Instant::now();
            }
            *record.lock().unwrap() = Some(now);
            Box::pin(async { Ok(Some("y".to_string())) })
        });
        let gate = CliApprovalGate::new(ApprovalPolicy::Ask, true, notifications.clone())
            .with_read_line(read_line);

        gate.authorize(&secret_request()).await.unwrap();
        notifications.drain(DRAIN_TIMEOUT).await;

        let asked_at = asked_at.lock().unwrap().expect("the gate never asked");
        let restarted = notifications.last_engaged();
        assert!(
            restarted >= asked_at,
            "the clock still runs from the start of the turn: \
             started {turn_started:?}, asked {asked_at:?}, clock {restarted:?}"
        );
    }

    #[tokio::test]
    async fn after_approving_all_later_calls_neither_prompt_nor_page() {
        let notifier = Arc::new(RecordingNotifier::default());
        let asked = Arc::new(AtomicUsize::new(0));
        let count = asked.clone();
        let read_line: ReadLine = Box::new(move |_| {
            count.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Ok(Some("a".to_string())) })
        });
        let (gate, notifications) = prompting(&notifier, read_line);

        for _ in 0..3 {
            let decision = gate.authorize(&secret_request()).await.unwrap();
            assert_eq!(decision, ApprovalDecision::Approve);
        }
        notifications.drain(DRAIN_TIMEOUT).await;

        assert_eq!(asked.load(Ordering::SeqCst), 1, "asked more than once");
        let sent = notifier.sent.lock().unwrap();
        assert_eq!(sent.len(), 1, "paged without a prompt: {sent:?}");
    }

    #[tokio::test]
    async fn an_unreadable_terminal_makes_the_gate_unavailable_rather_than_approving() {
        let notifier = Arc::new(RecordingNotifier::default());
        let read_line: ReadLine =
            Box::new(|_| Box::pin(async { Err(anyhow::anyhow!("stdin is gone")) }));
        let (gate, notifications) = prompting(&notifier, read_line);

        let result = gate.authorize(&secret_request()).await;
        notifications.drain(DRAIN_TIMEOUT).await;

        match result {
            Err(ApprovalError::Unavailable(reason)) => {
                assert!(reason.contains("stdin is gone"), "{reason}")
            }
            other => panic!("expected the gate to be unavailable, got {other:?}"),
        }
    }
}
