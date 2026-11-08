//! The HTTP side of notifications, and the three bodies it can send.
//!
//! The text of a notification is written here rather than in the domain
//! because it is presentation for one particular channel: Slack needs its
//! control characters escaped, ntfy wants a title separate from the message,
//! and each service caps the body at a different size.

use std::time::Duration;

use agent_domain::error::NotifyError;
use agent_domain::ports::events::FinishReason;
use agent_domain::ports::notify::{Notification, NotificationEvent, NotificationKind, Notifier};
use agent_domain::text;
use async_trait::async_trait;
use serde::Serialize;
use serde_json::{Value, json};

use super::{Endpoint, NotifyFormat, WebhookUrl, endpoint};
use crate::config::ConfigError;

/// ntfy rejects messages of 4096 bytes and over (it turns them into an
/// attachment). The margin leaves room for the ellipsis `clip` appends.
const NTFY_MAX_MESSAGE_BYTES: usize = 4000;
/// Slack truncates `text` well before its hard limit; 3000 stays readable.
const SLACK_MAX_TEXT_BYTES: usize = 3000;
const JSON_MAX_MESSAGE_BYTES: usize = 4000;

/// Shown in place of the session id. Enough to tell two sessions apart on a
/// phone, without making the full id - the name of the log file - public.
const SESSION_ID_CHARS: usize = 8;

pub struct WebhookNotifier {
    client: reqwest::Client,
    endpoint: Endpoint,
    format: NotifyFormat,
    timeout: Duration,
}

impl WebhookNotifier {
    pub fn new(
        url: &WebhookUrl,
        format: NotifyFormat,
        timeout: Duration,
    ) -> Result<Self, ConfigError> {
        let endpoint = endpoint(url, format).map_err(|reason| {
            ConfigError::invalid("AGENT_NOTIFY_WEBHOOK_URL", url.redacted(), reason)
        })?;
        let client = reqwest::Client::builder()
            .timeout(timeout)
            .user_agent(concat!("my-agent/", env!("CARGO_PKG_VERSION")))
            // A 307 would replay the body - model-written text included - to
            // wherever the server points. A webhook that redirects is
            // misconfigured, and the operator learns that from the status.
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|error| {
                ConfigError::invalid(
                    "AGENT_NOTIFY_WEBHOOK_URL",
                    url.redacted(),
                    format!("cannot build the HTTP client: {}", error.without_url()),
                )
            })?;

        Ok(Self {
            client,
            endpoint,
            format,
            timeout,
        })
    }

    fn transport(&self, error: reqwest::Error) -> NotifyError {
        if error.is_timeout() {
            return NotifyError::Timeout {
                seconds: self.timeout.as_secs(),
            };
        }
        // `reqwest::Error`'s Display ends in "for url (...)". The source
        // chain is not printed, so stripping the URL is enough.
        NotifyError::Transport {
            message: error.without_url().to_string(),
        }
    }
}

#[async_trait]
impl Notifier for WebhookNotifier {
    async fn notify(&self, notification: &Notification) -> Result<(), NotifyError> {
        let body = payload(self.format, notification, self.endpoint.topic.as_deref());

        let response = self
            .client
            .post(self.endpoint.url.clone())
            .json(&body)
            .send()
            .await
            .map_err(|error| self.transport(error))?;

        // The response body is never read: some services echo the request,
        // and nothing in it would change what happens next.
        let status = response.status();
        if !status.is_success() {
            return Err(NotifyError::HttpStatus {
                status: status.as_u16(),
            });
        }
        Ok(())
    }
}

/// The exact JSON body sent for `notification` in `format`.
pub(crate) fn payload(
    format: NotifyFormat,
    notification: &Notification,
    topic: Option<&str>,
) -> Value {
    let title = title(notification.kind());
    let message = message(notification);

    match format {
        NotifyFormat::Ntfy => json!({
            "topic": topic.unwrap_or_default(),
            "title": title,
            "message": text::clip(&message, NTFY_MAX_MESSAGE_BYTES),
            "tags": [tag(notification.kind())],
        }),
        NotifyFormat::Slack => json!({
            "text": text::clip(
                &slack_escape(&format!("*{title}*\n{message}")),
                SLACK_MAX_TEXT_BYTES,
            ),
            // Unfurling makes Slack's servers fetch every link in the text the
            // moment it is posted. With a model-written excerpt in there, that
            // is an exfiltration channel that needs no click.
            "unfurl_links": false,
            "unfurl_media": false,
        }),
        NotifyFormat::Json => {
            serde_json::to_value(JsonPayload::new(notification, title, &message))
                // Every field is a string or an integer, so serialisation
                // cannot fail; an empty object is the safe thing if it did.
                .unwrap_or_else(|_| json!({}))
        }
    }
}

/// Wire shape of the generic format. Separate from the domain type so that
/// renaming a domain field does not silently change what receivers parse.
#[derive(Serialize)]
struct JsonPayload<'a> {
    session_id: &'a str,
    kind: &'static str,
    title: &'static str,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    elapsed_secs: Option<u64>,
}

impl<'a> JsonPayload<'a> {
    fn new(notification: &'a Notification, title: &'static str, message: &str) -> Self {
        let (tool, elapsed) = match &notification.event {
            NotificationEvent::ApprovalPending { tool, .. } => (Some(tool.as_str()), None),
            NotificationEvent::TurnFinished { elapsed, .. }
            | NotificationEvent::TurnFailed { elapsed, .. } => (None, Some(elapsed.as_secs())),
        };
        Self {
            session_id: &notification.session_id,
            kind: notification.kind().as_str(),
            title,
            message: text::clip(message, JSON_MAX_MESSAGE_BYTES),
            tool,
            elapsed_secs: elapsed,
        }
    }
}

fn title(kind: NotificationKind) -> &'static str {
    match kind {
        NotificationKind::Approval => "agent: waiting for approval",
        NotificationKind::Completed => "agent: turn finished",
        NotificationKind::Failed => "agent: turn did not finish",
        // Never sent (the policy refuses it); named so the match stays whole.
        NotificationKind::Interrupted => "agent: turn interrupted",
    }
}

/// An emoji shortcode ntfy shows next to the title.
fn tag(kind: NotificationKind) -> &'static str {
    match kind {
        NotificationKind::Approval => "bell",
        NotificationKind::Completed => "white_check_mark",
        NotificationKind::Failed => "warning",
        NotificationKind::Interrupted => "stop_sign",
    }
}

fn message(notification: &Notification) -> String {
    let body = match &notification.event {
        NotificationEvent::ApprovalPending { tool, safety } => format!(
            "The {} tool `{tool}` is waiting for your answer.",
            safety.label()
        ),
        NotificationEvent::TurnFinished {
            reason,
            elapsed,
            excerpt,
        } => {
            let elapsed = human(*elapsed);
            let mut body = match reason {
                FinishReason::Completed => format!("Finished in {elapsed}."),
                FinishReason::MaxIterations { limit } => format!(
                    "Stopped after {elapsed}: reached the iteration limit ({limit}) \
                     without an answer."
                ),
                FinishReason::Stopped { stop_reason } => format!(
                    "Stopped after {elapsed}: the provider ended the turn ({}).",
                    text::truncate(stop_reason, 64)
                ),
                FinishReason::Interrupted => format!("Interrupted by the user after {elapsed}."),
            };
            if let Some(excerpt) = excerpt {
                body.push_str("\n\n");
                body.push_str(&defang_urls(excerpt));
            }
            body
        }
        NotificationEvent::TurnFailed { elapsed, reason } => {
            format!("Failed after {}: {reason}", human(*elapsed))
        }
    };

    let session: String = notification
        .session_id
        .chars()
        .take(SESSION_ID_CHARS)
        .collect();
    format!("{body}\nsession {session}")
}

/// `42s`, `3m 05s`, `1h 07m`.
fn human(elapsed: Duration) -> String {
    let secs = elapsed.as_secs();
    match secs {
        0..60 => format!("{secs}s"),
        60..3600 => format!("{}m {:02}s", secs / 60, secs % 60),
        _ => format!("{}h {:02}m", secs / 3600, secs % 3600 / 60),
    }
}

/// Breaks `scheme://` in model-written text so no client turns it into a link.
///
/// A prompt-injected answer can carry `https://attacker/?k=<secret>`: Slack
/// would fetch it to build a preview, and on ntfy it is one tap away. U+2044
/// (fraction slash) reads the same to a person and is not a URL separator to
/// any linkifier. Bare domains without a scheme are not caught; the clients'
/// fetch-on-post behaviour is what this closes, and Slack's is also switched
/// off in the payload.
fn defang_urls(raw: &str) -> String {
    raw.replace("://", ":\u{2044}\u{2044}")
}

/// Slack reads `<...>` as a link or a mention, so a model-written `<!channel>`
/// in an excerpt would page everyone in the channel. Its documentation names
/// exactly these three characters as the ones to escape.
fn slack_escape(raw: &str) -> String {
    raw.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_domain::model::tool::{ToolName, ToolSafety};

    fn finished(excerpt: Option<&str>) -> Notification {
        Notification {
            session_id: "0123456789abcdef".into(),
            event: NotificationEvent::TurnFinished {
                reason: FinishReason::Completed,
                elapsed: Duration::from_secs(125),
                excerpt: excerpt.map(String::from),
            },
        }
    }

    #[test]
    fn durations_read_the_way_a_person_would_say_them() {
        assert_eq!(human(Duration::from_secs(42)), "42s");
        assert_eq!(human(Duration::from_secs(185)), "3m 05s");
        assert_eq!(human(Duration::from_secs(4020)), "1h 07m");
    }

    #[test]
    fn only_a_prefix_of_the_session_id_is_sent() {
        let body = message(&finished(None));
        assert!(body.ends_with("session 01234567"), "{body}");
        assert!(!body.contains("89abcdef"), "{body}");
    }

    #[test]
    fn slack_mentions_in_model_text_are_neutralised() {
        let value = payload(
            NotifyFormat::Slack,
            &finished(Some("done <!channel> & <@U123>")),
            None,
        );
        let text = value["text"].as_str().unwrap();
        assert!(!text.contains("<!channel>"), "{text}");
        assert!(
            text.contains("&lt;!channel&gt; &amp; &lt;@U123&gt;"),
            "{text}"
        );
    }

    #[test]
    fn urls_in_the_excerpt_are_broken_in_every_format() {
        let leak = finished(Some("see https://x.example/?k=secret"));
        for format in [NotifyFormat::Ntfy, NotifyFormat::Slack, NotifyFormat::Json] {
            let body = payload(format, &leak, Some("t")).to_string();
            assert!(!body.contains("://"), "{format:?}: {body}");
            assert!(body.contains("x.example/?k=secret"), "{format:?}: {body}");
        }
    }

    #[test]
    fn ntfy_messages_stay_under_the_attachment_threshold() {
        let long = "あ".repeat(3000); // 9000 bytes
        let value = payload(NotifyFormat::Ntfy, &finished(Some(&long)), Some("t"));
        let message = value["message"].as_str().unwrap();
        assert!(message.len() < 4096, "{} bytes", message.len());
    }

    #[test]
    fn an_approval_names_the_tool_and_its_class() {
        let notification = Notification {
            session_id: "s".into(),
            event: NotificationEvent::ApprovalPending {
                tool: ToolName::new("run_command").unwrap(),
                safety: ToolSafety::Destructive,
            },
        };
        let body = message(&notification);
        assert!(body.contains("`run_command`"), "{body}");
        assert!(body.contains("destructive"), "{body}");
    }
}
