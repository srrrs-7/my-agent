//! Push notifications: what is worth sending, and sending it without making
//! the agent wait.
//!
//! The decisions live here as plain functions because the places that act on
//! them - the composition root and the subcommands - have no tests. What gets
//! sent, when a turn is too short to mention, and how much of an answer leaves
//! the machine are all answered below and checked at the bottom of this file.
//!
//! [`Notifications`] is the one object the approval gate and the subcommands
//! share. It owns the "last time a human did something" clock: a turn's
//! duration is measured from there, because an hour spent waiting on an
//! unanswered prompt is not an hour of work, and resetting the clock when the
//! prompt is answered is what stops the completion that follows from being
//! reported as a long-running task.
//!
//! Delivery never blocks and never fails anything. A notification is spawned
//! and forgotten; a failed one is a single warning on stderr.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use agent_application::AppError;
use agent_application::agent::AgentOutcome;
use agent_domain::error::LlmError;
use agent_domain::ports::approval::ApprovalRequest;
use agent_domain::ports::events::FinishReason;
use agent_domain::ports::notify::{Notification, NotificationEvent, NotificationKind, Notifier};
use agent_infrastructure::config::NotifySettings;
use tokio::task::JoinSet;
use tracing::warn;

/// How long `agent run` waits for its last notification before exiting.
/// A little over the adapter's own request timeout, so a slow endpoint
/// fails as a timeout warning rather than by being cut off silently.
pub const DRAIN_TIMEOUT: Duration = Duration::from_secs(6);

/// Which notifications the operator asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotifyPolicy {
    events: Vec<NotificationKind>,
    min_turn: Duration,
    excerpt_chars: usize,
}

impl NotifyPolicy {
    pub fn from_settings(settings: &NotifySettings) -> Self {
        Self {
            events: settings.events.clone(),
            min_turn: settings.min_turn,
            excerpt_chars: settings.excerpt_chars,
        }
    }

    /// Whether `notification` is worth sending.
    ///
    /// The duration threshold applies to completions only. A prompt waiting
    /// for an answer and a turn that gave up both need the human however
    /// quickly they happened.
    pub fn wants(&self, notification: &Notification) -> bool {
        let kind = notification.kind();
        if !self.events.contains(&kind) {
            return false;
        }
        match (&notification.event, kind) {
            (NotificationEvent::TurnFinished { elapsed, .. }, NotificationKind::Completed) => {
                *elapsed >= self.min_turn
            }
            _ => true,
        }
    }
}

/// The notification for a confirmation prompt.
///
/// `request.summary` is left out on purpose: it quotes the model's arguments
/// (up to several kilobytes of file contents, a shell command, a URL), and
/// none of that should reach a chat channel unread.
pub fn approval_pending(session_id: &str, request: &ApprovalRequest) -> Notification {
    Notification {
        session_id: session_id.to_string(),
        event: NotificationEvent::ApprovalPending {
            tool: request.call.name.clone(),
            safety: request.safety,
        },
    }
}

/// The notification for whatever the loop returned.
///
/// Only a final answer carries an excerpt. A turn that ran out of iterations
/// has no answer to quote, only whatever the model was saying when it was cut
/// off.
pub fn turn_outcome(
    session_id: &str,
    result: &Result<AgentOutcome, AppError>,
    elapsed: Duration,
    excerpt_chars: usize,
) -> Notification {
    let event = match result {
        Ok(outcome) => NotificationEvent::TurnFinished {
            reason: outcome.reason.clone(),
            elapsed,
            excerpt: match outcome.reason {
                FinishReason::Completed => excerpt(&outcome.final_text, excerpt_chars),
                _ => None,
            },
        },
        Err(error) => NotificationEvent::TurnFailed {
            elapsed,
            reason: failure_reason(error),
        },
    };
    Notification {
        session_id: session_id.to_string(),
        event,
    }
}

/// A coarse description of why a turn failed.
///
/// Never the error's own text: an `LlmError` can carry the provider's base
/// URL and up to 500 bytes of its response body, and a filesystem error names
/// paths. The terminal already shows the full error; the phone only needs to
/// know that it should go and look.
fn failure_reason(error: &AppError) -> String {
    match error {
        AppError::Llm(LlmError::Timeout { .. }) => "the model request timed out".into(),
        AppError::Llm(LlmError::RateLimited { .. }) => "the provider is rate limiting".into(),
        AppError::Llm(LlmError::Auth(_)) => "the provider rejected the credentials".into(),
        AppError::Llm(LlmError::Api { status, .. }) => {
            format!("the provider answered HTTP {status}")
        }
        AppError::Llm(_) => "the model request failed".into(),
        AppError::FileSystem(_) => "a workspace file error stopped the turn".into(),
    }
}

/// The first `max_chars` characters of `text`, with `…` when it was cut.
///
/// Counted in characters rather than bytes because the setting is read by a
/// person: "100" should mean the same amount of Japanese as of English. The
/// adapter still clips to its service's byte limit on top of this.
fn excerpt(text: &str, max_chars: usize) -> Option<String> {
    let text = text.trim();
    if max_chars == 0 || text.is_empty() {
        return None;
    }
    let mut chars = text.chars();
    let mut head: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        head.push('…');
    }
    Some(head)
}

/// The shared sender. One instance per process, handed to the approval gate
/// and to whichever subcommand runs.
pub struct Notifications {
    notifier: Arc<dyn Notifier>,
    policy: NotifyPolicy,
    /// `std` rather than `tokio` mutexes: none is held across an `.await`.
    pending: Mutex<JoinSet<()>>,
    last_engaged: Mutex<Instant>,
    /// The approval gate is called without a session, so the subcommand
    /// records which one is running.
    session_id: Mutex<String>,
}

impl Notifications {
    pub fn new(notifier: Arc<dyn Notifier>, policy: NotifyPolicy) -> Self {
        Self {
            notifier,
            policy,
            pending: Mutex::new(JoinSet::new()),
            last_engaged: Mutex::new(Instant::now()),
            session_id: Mutex::new(String::new()),
        }
    }

    /// Sends nothing. What the approval gate's tests run against.
    #[cfg(test)]
    pub fn disabled() -> Self {
        Self::new(
            Arc::new(agent_domain::ports::notify::NullNotifier),
            NotifyPolicy {
                events: Vec::new(),
                min_turn: Duration::ZERO,
                excerpt_chars: 0,
            },
        )
    }

    /// A human just handed the agent a prompt in `session_id`.
    pub fn begin_turn(&self, session_id: &str) {
        *lock(&self.session_id) = session_id.to_string();
        self.engaged();
    }

    /// A human just answered something. The clock for "was this turn long?"
    /// restarts here.
    pub fn engaged(&self) {
        self.engaged_at(Instant::now());
    }

    /// When the turn clock last restarted.
    #[cfg(test)]
    pub(crate) fn last_engaged(&self) -> Instant {
        *lock(&self.last_engaged)
    }

    // The `_at` variants exist so tests can move time forward by adding to an
    // `Instant`. Subtracting from `now()` instead panics on a machine that
    // booted more recently than the offset - a freshly started container VM.
    fn engaged_at(&self, now: Instant) {
        *lock(&self.last_engaged) = now;
    }

    /// A confirmation prompt is about to be shown.
    pub fn approval_pending(&self, request: &ApprovalRequest) {
        let session_id = lock(&self.session_id).clone();
        self.send(approval_pending(&session_id, request));
    }

    /// The loop returned.
    pub fn turn_finished(&self, result: &Result<AgentOutcome, AppError>) {
        self.turn_finished_at(result, Instant::now());
    }

    fn turn_finished_at(&self, result: &Result<AgentOutcome, AppError>, now: Instant) {
        let session_id = lock(&self.session_id).clone();
        let elapsed = now.saturating_duration_since(*lock(&self.last_engaged));
        self.send(turn_outcome(
            &session_id,
            result,
            elapsed,
            self.policy.excerpt_chars,
        ));
    }

    /// Starts delivering `notification` if the policy wants it, and returns
    /// at once.
    pub fn send(&self, notification: Notification) {
        if !self.policy.wants(&notification) {
            return;
        }
        let notifier = self.notifier.clone();
        let mut pending = lock(&self.pending);
        // Reap what already finished, so a long chat does not accumulate one
        // entry per turn.
        while pending.try_join_next().is_some() {}
        pending.spawn(async move {
            if let Err(error) = notifier.notify(&notification).await {
                // The error carries no URL by contract (`NotifyError`).
                warn!(kind = notification.kind().as_str(), %error, "notification not delivered");
            }
        });
    }

    /// Waits up to `timeout` for deliveries still in flight. Whatever has not
    /// finished by then is abandoned with one warning.
    pub async fn drain(&self, timeout: Duration) {
        let mut pending = std::mem::take(&mut *lock(&self.pending));
        let finished = tokio::time::timeout(timeout, async {
            while pending.join_next().await.is_some() {}
        })
        .await;
        if finished.is_err() {
            warn!(
                abandoned = pending.len(),
                "gave up waiting for notifications to be delivered"
            );
        }
    }
}

/// A poisoned lock only means another thread panicked while holding it. The
/// values behind these locks are always valid on their own, so carrying on is
/// safe, and a notification is never a reason to panic a second time.
fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use agent_domain::error::{FsError, NotifyError};
    use agent_domain::model::llm::TokenUsage;
    use agent_domain::model::tool::{ToolCall, ToolCallId, ToolName, ToolSafety};
    use async_trait::async_trait;
    use serde_json::json;

    /// Remembers what it was asked to send.
    #[derive(Default)]
    pub(crate) struct RecordingNotifier {
        pub(crate) sent: Mutex<Vec<Notification>>,
        pub(crate) fail: bool,
    }

    #[async_trait]
    impl Notifier for RecordingNotifier {
        async fn notify(&self, notification: &Notification) -> Result<(), NotifyError> {
            lock(&self.sent).push(notification.clone());
            if self.fail {
                return Err(NotifyError::HttpStatus { status: 500 });
            }
            Ok(())
        }
    }

    pub(crate) fn everything() -> NotifyPolicy {
        NotifyPolicy {
            events: NotificationKind::ALL.to_vec(),
            min_turn: Duration::from_secs(30),
            excerpt_chars: 100,
        }
    }

    fn outcome(reason: FinishReason, text: &str) -> Result<AgentOutcome, AppError> {
        Ok(AgentOutcome {
            final_text: text.to_string(),
            iterations: 1,
            usage: TokenUsage::default(),
            reason,
        })
    }

    fn completed_after(secs: u64) -> Notification {
        turn_outcome(
            "s",
            &outcome(FinishReason::Completed, "done"),
            Duration::from_secs(secs),
            100,
        )
    }

    fn request() -> ApprovalRequest {
        ApprovalRequest {
            call: ToolCall::new(
                ToolCallId::new("1"),
                ToolName::new("write_file").unwrap(),
                json!({"path": "a.rs", "content": "API_KEY=sk-live-123"}),
            ),
            safety: ToolSafety::Mutating,
            summary: "write_file(path=\"a.rs\", content=\"API_KEY=sk-live-123\")".into(),
        }
    }

    #[test]
    fn short_completed_turns_are_not_worth_a_notification() {
        let policy = everything();
        assert!(!policy.wants(&completed_after(29)));
        assert!(policy.wants(&completed_after(30)));
    }

    #[test]
    fn the_threshold_does_not_hold_back_failures_or_prompts() {
        let policy = everything();
        let gave_up = turn_outcome(
            "s",
            &outcome(FinishReason::MaxIterations { limit: 3 }, ""),
            Duration::from_secs(1),
            100,
        );
        let broke = turn_outcome(
            "s",
            &Err(AppError::Llm(LlmError::Timeout { seconds: 1 })),
            Duration::ZERO,
            100,
        );
        assert!(policy.wants(&gave_up));
        assert!(policy.wants(&broke));
        assert!(policy.wants(&approval_pending("s", &request())));
    }

    #[test]
    fn unsubscribed_kinds_are_dropped() {
        let policy = NotifyPolicy {
            events: vec![NotificationKind::Failed],
            ..everything()
        };
        assert!(!policy.wants(&completed_after(3600)));
        assert!(!policy.wants(&approval_pending("s", &request())));
    }

    #[test]
    fn an_approval_names_the_tool_but_carries_none_of_its_arguments() {
        let notification = approval_pending("s", &request());
        assert_eq!(
            notification.event,
            NotificationEvent::ApprovalPending {
                tool: ToolName::new("write_file").unwrap(),
                safety: ToolSafety::Mutating,
            }
        );
        assert!(
            !format!("{notification:?}").contains("sk-live"),
            "the summary must not travel: {notification:?}"
        );
    }

    #[test]
    fn the_excerpt_is_cut_at_the_character_limit() {
        assert_eq!(excerpt("  hello world  ", 5).as_deref(), Some("hello…"));
        assert_eq!(excerpt("hello", 5).as_deref(), Some("hello"));
        // Counted in characters: three Japanese characters are nine bytes.
        assert_eq!(excerpt("こんにちは", 3).as_deref(), Some("こんに…"));
    }

    #[test]
    fn a_zero_limit_or_an_empty_answer_sends_no_excerpt() {
        assert_eq!(excerpt("anything", 0), None);
        assert_eq!(excerpt("  \n ", 100), None);
    }

    #[test]
    fn only_a_completed_turn_quotes_the_answer() {
        let gave_up = turn_outcome(
            "s",
            &outcome(FinishReason::MaxIterations { limit: 3 }, "half a thought"),
            Duration::ZERO,
            100,
        );
        assert!(
            matches!(
                gave_up.event,
                NotificationEvent::TurnFinished { excerpt: None, .. }
            ),
            "{gave_up:?}"
        );
        assert_eq!(gave_up.kind(), NotificationKind::Failed);

        match completed_after(1).event {
            NotificationEvent::TurnFinished { excerpt, .. } => {
                assert_eq!(excerpt.as_deref(), Some("done"))
            }
            other => panic!("expected a finished turn, got {other:?}"),
        }
    }

    #[test]
    fn a_failure_is_described_without_the_error_text() {
        let error = AppError::Llm(LlmError::Transport(
            "connection refused (http://10.0.0.5:11434/v1?key=secret)".into(),
        ));
        let notification = turn_outcome("s", &Err(error), Duration::ZERO, 100);
        let NotificationEvent::TurnFailed { reason, .. } = &notification.event else {
            panic!("expected a failure, got {notification:?}");
        };
        assert_eq!(reason, "the model request failed");

        let error = AppError::FileSystem(FsError::NotFound {
            path: "/home/me/secret-plans.md".into(),
        });
        let notification = turn_outcome("s", &Err(error), Duration::ZERO, 100);
        assert!(!format!("{notification:?}").contains("secret-plans"));
    }

    #[tokio::test]
    async fn a_failed_delivery_is_absorbed_and_drain_returns() {
        let notifier = Arc::new(RecordingNotifier {
            fail: true,
            ..Default::default()
        });
        let notifications = Notifications::new(notifier.clone(), everything());
        notifications.begin_turn("session-1");

        notifications.turn_finished(&Err(AppError::Llm(LlmError::Timeout { seconds: 1 })));
        notifications.drain(DRAIN_TIMEOUT).await;

        let sent = lock(&notifier.sent);
        assert_eq!(sent.len(), 1, "{sent:?}");
        assert_eq!(sent[0].session_id, "session-1");
    }

    #[tokio::test]
    async fn the_turn_clock_restarts_when_the_human_answers() {
        // A prompt sat unanswered for most of an hour, then the model needed
        // ten more seconds.
        let notifier = Arc::new(RecordingNotifier::default());
        let notifications = Notifications::new(notifier.clone(), everything());
        notifications.begin_turn("s");
        let started = *lock(&notifications.last_engaged);

        notifications.engaged_at(started + Duration::from_secs(3590));
        notifications.turn_finished_at(
            &outcome(FinishReason::Completed, "ok"),
            started + Duration::from_secs(3600),
        );
        notifications.drain(DRAIN_TIMEOUT).await;

        assert!(
            lock(&notifier.sent).is_empty(),
            "the hour spent waiting on the human is not a long turn"
        );
    }

    #[tokio::test]
    async fn a_long_turn_is_reported_with_its_session() {
        let notifier = Arc::new(RecordingNotifier::default());
        let notifications = Notifications::new(notifier.clone(), everything());
        notifications.begin_turn("s-42");
        let started = *lock(&notifications.last_engaged);

        notifications.turn_finished_at(
            &outcome(FinishReason::Completed, "all done"),
            started + Duration::from_secs(120),
        );
        notifications.drain(DRAIN_TIMEOUT).await;

        let sent = lock(&notifier.sent);
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].session_id, "s-42");
        assert_eq!(sent[0].kind(), NotificationKind::Completed);
    }
}
