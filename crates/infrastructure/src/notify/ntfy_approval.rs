//! Answering a prompt from an ntfy notification.
//!
//! The notification carries two buttons. Each makes the ntfy app `POST`
//! `approve <token>` or `deny <token>` to a second, reply topic, which this
//! adapter reads through an outbound subscription - no port is opened here.
//!
//! **Who can approve.** ntfy's buttons send a fixed body, so whatever a reply
//! needs is in the notification, and anyone who can read the notification
//! topic can approve. Signing replies would change nothing: the signature
//! would sit in the same button. The protection is therefore the server's
//! access control, and it is *checked*, not assumed: [`NtfyRemoteApprover::connect`]
//! tries, without credentials, to read the notification topic and to write to
//! the reply topic, and refuses to start when either works - the same rule as
//! a sandbox that is weaker than asked for. The ntfy server itself is trusted.
//!
//! **What the token is for.** The per-prompt token is 128 random bits, valid
//! for one answer to one prompt and for a limited time. It stops a reply
//! meant for an earlier prompt, a replayed reply and a guessed reply; it does
//! not stop a reader of the notification topic, which is what the access
//! check above is for.
//!
//! **Order.** The subscription is opened before the notification is sent, and
//! asks for messages since the moment of asking, so a fast tap cannot land
//! in the gap between the two.
//!
//! Nothing here denies anything. A failure, an expired token or a reply that
//! does not fit ends or skips this channel; the terminal is still waiting.

use std::io::Read as _;
use std::pin::Pin;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use agent_domain::error::RemoteApprovalError;
use agent_domain::ports::remote_approval::{
    RemoteAnswer, RemoteApprovalRequest, RemoteApprover, TerminalOnlyNotice,
};
use agent_domain::text;
use async_trait::async_trait;
use bytes::Bytes;
use futures::{Stream, StreamExt};
use reqwest::StatusCode;
use reqwest::header::AUTHORIZATION;
use serde::Deserialize;
use serde_json::{Value, json};
use thiserror::Error;
use tracing::warn;
use url::Url;

use super::webhook::defang_urls;
use super::{DEFAULT_TIMEOUT, NotifyFormat, NtfyToken, endpoint};
use crate::config::NotifySettings;

/// Bytes of randomness per prompt.
const TOKEN_BYTES: usize = 16;

/// Same limit as ordinary ntfy notifications (see `webhook.rs`).
const MAX_MESSAGE_BYTES: usize = 4000;

/// A reply line longer than this is not a reply. ntfy's own messages are
/// capped at 4 KB; this only stops a broken server from growing the buffer.
const MAX_LINE_BYTES: usize = 64 * 1024;

/// ntfy sends a keepalive every 45 seconds. A subscription silent for much
/// longer than that is dead, even if the connection looks open.
const IDLE_LIMIT: Duration = Duration::from_secs(150);

/// Characters of the session id shown, as in ordinary notifications.
const SESSION_ID_CHARS: usize = 8;

/// Why remote approval cannot start. Each one is for the operator, and none
/// repeats a topic or a token.
#[derive(Debug, Error)]
pub enum RemoteApprovalSetupError {
    #[error("{0}")]
    Config(String),

    #[error(
        "the notification topic can be read without a token. Anyone who can read it can \
         approve tool calls with its buttons - deny anonymous access on the ntfy server \
         (e.g. auth-default-access: deny-all) or unset AGENT_NOTIFY_NTFY_REPLY_TOPIC"
    )]
    AnonymousCanRead,

    #[error(
        "the reply topic accepts messages without a token. Deny anonymous access on the ntfy \
         server (e.g. auth-default-access: deny-all) or unset AGENT_NOTIFY_NTFY_REPLY_TOPIC"
    )]
    AnonymousCanWrite,

    #[error(
        "could not confirm that {what} is refused without a token ({reason}). Remote approval \
         is only started when it is - check the ntfy server's address and access rules, or \
         unset AGENT_NOTIFY_NTFY_REPLY_TOPIC"
    )]
    Unproven { what: &'static str, reason: String },

    #[error(
        "AGENT_NOTIFY_NTFY_TOKEN cannot read the reply topic ({reason}). Grant it read access \
         (ntfy access <user> <reply topic> read-write) or fix the token"
    )]
    TokenCannotRead { reason: String },
}

pub struct NtfyRemoteApprover {
    /// Short requests: the startup checks and publishing. Bounded overall.
    client: reqwest::Client,
    /// The subscription, which is meant to stay open for as long as the
    /// prompt waits - a client with an overall timeout would cut it off.
    subscriber: reqwest::Client,
    /// The server root, ending in `/`.
    root: Url,
    notify_topic: String,
    reply_topic: String,
    token: NtfyToken,
    /// What the buttons carry. The main token when no separate one is set.
    button_token: NtfyToken,
    ttl: Duration,
}

impl NtfyRemoteApprover {
    /// Builds the adapter and proves the server refuses anonymous access to
    /// both topics. `Ok(None)` when remote approval is not configured.
    pub async fn connect(
        settings: &NotifySettings,
    ) -> Result<Option<Self>, RemoteApprovalSetupError> {
        let Some(remote) = &settings.remote_approval else {
            return Ok(None);
        };
        // Config already enforces all three; repeated so this type cannot be
        // built half-configured by another caller.
        let (Some(url), NotifyFormat::Ntfy, Some(token)) =
            (&settings.webhook, settings.format, &settings.ntfy_token)
        else {
            return Err(RemoteApprovalSetupError::Config(
                "remote approval needs an ntfy AGENT_NOTIFY_WEBHOOK_URL and \
                 AGENT_NOTIFY_NTFY_TOKEN"
                    .into(),
            ));
        };
        let endpoint =
            endpoint(url, NotifyFormat::Ntfy).map_err(RemoteApprovalSetupError::Config)?;
        let notify_topic = endpoint
            .topic
            .ok_or_else(|| RemoteApprovalSetupError::Config("the ntfy URL has no topic".into()))?;

        let builder = || {
            reqwest::Client::builder()
                .user_agent(concat!("my-agent/", env!("CARGO_PKG_VERSION")))
                // A redirect would replay the token to wherever it points.
                .redirect(reqwest::redirect::Policy::none())
        };
        let client = builder()
            .timeout(DEFAULT_TIMEOUT)
            .build()
            .map_err(|error| RemoteApprovalSetupError::Config(client_error(&error)))?;
        let subscriber = builder()
            .connect_timeout(DEFAULT_TIMEOUT)
            .build()
            .map_err(|error| RemoteApprovalSetupError::Config(client_error(&error)))?;

        let approver = Self {
            client,
            subscriber,
            root: endpoint.url,
            notify_topic,
            reply_topic: remote.reply_topic.clone(),
            token: token.clone(),
            button_token: remote.reply_token.clone().unwrap_or_else(|| token.clone()),
            ttl: remote.ttl,
        };
        approver.prove_access_control().await?;
        Ok(Some(approver))
    }

    /// The startup checks. Anything other than an explicit refusal - a
    /// success, a redirect, a 5xx, no answer - fails, because none of those
    /// shows that a stranger is kept out.
    async fn prove_access_control(&self) -> Result<(), RemoteApprovalSetupError> {
        let read = self
            .client
            .get(self.topic_url(&self.notify_topic, "json"))
            .query(&[("poll", "1")])
            .send()
            .await;
        match refusal(read) {
            Ok(()) => {}
            Err(Probe::Allowed) => return Err(RemoteApprovalSetupError::AnonymousCanRead),
            Err(Probe::Unclear(reason)) => {
                return Err(RemoteApprovalSetupError::Unproven {
                    what: "reading the notification topic",
                    reason,
                });
            }
        }

        // If this goes through, a stray line lands on the reply topic. It
        // is not shaped like a reply, and startup fails anyway.
        let write = self
            .client
            .post(self.topic_url(&self.reply_topic, ""))
            .body("my-agent startup check: anonymous write")
            .send()
            .await;
        match refusal(write) {
            Ok(()) => {}
            Err(Probe::Allowed) => return Err(RemoteApprovalSetupError::AnonymousCanWrite),
            Err(Probe::Unclear(reason)) => {
                return Err(RemoteApprovalSetupError::Unproven {
                    what: "writing to the reply topic",
                    reason,
                });
            }
        }

        // Not a security check: without it, a token that cannot subscribe
        // would show up as a warning on every prompt instead of once, here.
        let own = self
            .client
            .get(self.topic_url(&self.reply_topic, "json"))
            .query(&[("poll", "1")])
            .header(AUTHORIZATION, self.token.bearer())
            .send()
            .await
            .map_err(|error| RemoteApprovalSetupError::TokenCannotRead {
                reason: error.without_url().to_string(),
            })?;
        if !own.status().is_success() {
            return Err(RemoteApprovalSetupError::TokenCannotRead {
                reason: format!("HTTP {}", own.status().as_u16()),
            });
        }
        Ok(())
    }

    /// `<root><topic>` or `<root><topic>/<suffix>`.
    fn topic_url(&self, topic: &str, suffix: &str) -> Url {
        let path = if suffix.is_empty() {
            topic.to_string()
        } else {
            format!("{topic}/{suffix}")
        };
        // Both parts were validated as [A-Za-z0-9_-] (or are constants), so
        // the join cannot fail; the root is the fallback that keeps this total.
        self.root.join(&path).unwrap_or_else(|_| self.root.clone())
    }

    async fn subscribe(&self, since: u64) -> Result<Replies, RemoteApprovalError> {
        let response = self
            .subscriber
            .get(self.topic_url(&self.reply_topic, "json"))
            .query(&[("since", since.to_string())])
            .header(AUTHORIZATION, self.token.bearer())
            .send()
            .await
            .map_err(transport)?;
        if !response.status().is_success() {
            return Err(RemoteApprovalError::HttpStatus {
                status: response.status().as_u16(),
            });
        }
        Ok(Replies::new(response.bytes_stream()))
    }

    async fn publish(&self, body: &Value) -> Result<(), RemoteApprovalError> {
        let response = self
            .client
            .post(self.root.clone())
            .header(AUTHORIZATION, self.token.bearer())
            .json(body)
            .send()
            .await
            .map_err(transport)?;
        if !response.status().is_success() {
            return Err(RemoteApprovalError::HttpStatus {
                status: response.status().as_u16(),
            });
        }
        Ok(())
    }

    /// The notification with the two buttons.
    fn approval_payload(&self, request: &RemoteApprovalRequest, token: &str) -> Value {
        let button = |label: &str, verb: &str| {
            json!({
                "action": "http",
                "label": label,
                "url": self.topic_url(&self.reply_topic, "").as_str(),
                "method": "POST",
                "headers": { "Authorization": self.button_token.bearer() },
                "body": format!("{verb} {token}"),
                "clear": true,
            })
        };
        let message = format!(
            "The {} tool `{}` is waiting for your answer:\n\n{}\n\nApprove or deny here, or \
             answer at the terminal. The buttons stop working after {} min.\n{}",
            request.safety.label(),
            request.tool,
            defang_urls(&request.detail),
            self.ttl.as_secs().div_ceil(60),
            session_line(&request.session_id),
        );
        json!({
            "topic": self.notify_topic,
            "title": "agent: waiting for approval",
            "message": text::clip(&message, MAX_MESSAGE_BYTES),
            "tags": ["bell"],
            "actions": [button("Approve", "approve"), button("Deny", "deny")],
        })
    }

    fn terminal_only_payload(&self, notice: &TerminalOnlyNotice) -> Value {
        json!({
            "topic": self.notify_topic,
            "title": "agent: waiting at the terminal",
            "message": format!(
                "The {} tool `{}` wants to change the project's dependencies. That can only be \
                 answered at the terminal.\n{}",
                notice.safety.label(),
                notice.tool,
                session_line(&notice.session_id),
            ),
            "tags": ["bell"],
        })
    }
}

#[async_trait]
impl RemoteApprover for NtfyRemoteApprover {
    fn is_active(&self) -> bool {
        true
    }

    async fn ask(
        &self,
        request: &RemoteApprovalRequest,
    ) -> Result<RemoteAnswer, RemoteApprovalError> {
        let mut pending = PendingApproval::new(one_time_token()?, Instant::now() + self.ttl);
        let since = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_secs());

        let mut replies = self.subscribe(since).await?;
        let payload = self.approval_payload(request, pending.token().unwrap_or_default());
        self.publish(&payload).await?;

        let expiry = tokio::time::sleep(self.ttl);
        tokio::pin!(expiry);
        loop {
            let line = tokio::select! {
                () = &mut expiry => {
                    // `pending` is dropped with the token in it: from here
                    // on, no reply to this prompt can be accepted.
                    return Err(RemoteApprovalError::Expired { seconds: self.ttl.as_secs() });
                }
                line = tokio::time::timeout(IDLE_LIMIT, replies.next_line()) => line,
            };
            let line = line.map_err(|_| RemoteApprovalError::Closed)??;
            let Some(message) = reply_message(&line, &self.reply_topic) else {
                continue;
            };
            match pending.judge(&message, Instant::now()) {
                Verdict::Answer(answer) => return Ok(answer),
                Verdict::Ignored(reason) => {
                    warn!(reason, "ignored a reply on the remote approval topic");
                }
            }
        }
    }

    async fn announce_terminal_only(
        &self,
        notice: &TerminalOnlyNotice,
    ) -> Result<(), RemoteApprovalError> {
        self.publish(&self.terminal_only_payload(notice)).await
    }
}

enum Probe {
    /// The server let an anonymous request through.
    Allowed,
    /// Neither a success nor a refusal.
    Unclear(String),
}

/// `Ok` only for an explicit refusal: 401 or 403.
fn refusal(response: reqwest::Result<reqwest::Response>) -> Result<(), Probe> {
    let response = response.map_err(|error| Probe::Unclear(error.without_url().to_string()))?;
    let status = response.status();
    if matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN) {
        Ok(())
    } else if status.is_success() {
        Err(Probe::Allowed)
    } else {
        Err(Probe::Unclear(format!("HTTP {}", status.as_u16())))
    }
}

fn transport(error: reqwest::Error) -> RemoteApprovalError {
    RemoteApprovalError::Transport {
        message: error.without_url().to_string(),
    }
}

fn client_error(error: &reqwest::Error) -> String {
    format!("cannot build the HTTP client: {error}")
}

fn session_line(session_id: &str) -> String {
    let session: String = session_id.chars().take(SESSION_ID_CHARS).collect();
    format!("session {session}")
}

/// 128 random bits from the kernel, as lowercase hex.
fn one_time_token() -> Result<String, RemoteApprovalError> {
    let mut bytes = [0_u8; TOKEN_BYTES];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut source| source.read_exact(&mut bytes))
        .map_err(|error| RemoteApprovalError::Token {
            message: error.to_string(),
        })?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

/// The `message` of an ntfy `message` event on `topic`, or `None` for
/// anything else (`open`, `keepalive`, lines that are not ntfy's).
fn reply_message(line: &str, topic: &str) -> Option<String> {
    #[derive(Deserialize)]
    struct Event {
        #[serde(default)]
        event: String,
        #[serde(default)]
        topic: Option<String>,
        #[serde(default)]
        message: Option<String>,
    }
    let event: Event = serde_json::from_str(line).ok()?;
    if event.event != "message" || event.topic.as_deref().is_some_and(|t| t != topic) {
        return None;
    }
    event.message
}

/// The one prompt waiting for a reply, and the token that answers it.
struct PendingApproval {
    /// `None` once used.
    token: Option<String>,
    expires: Instant,
}

#[derive(Debug, PartialEq, Eq)]
enum Verdict {
    Answer(RemoteAnswer),
    /// Why the reply was skipped. Never quotes the reply, which may be a
    /// near-miss of the token.
    Ignored(&'static str),
}

impl PendingApproval {
    fn new(token: String, expires: Instant) -> Self {
        Self {
            token: Some(token),
            expires,
        }
    }

    fn token(&self) -> Option<&str> {
        self.token.as_deref()
    }

    /// Decides what one reply means. The token is consumed by the first
    /// valid answer and by expiry, so nothing after either can count.
    fn judge(&mut self, message: &str, now: Instant) -> Verdict {
        let Some((verb, presented)) = message.trim().split_once(' ') else {
            return Verdict::Ignored("not a reply: expected `approve <token>` or `deny <token>`");
        };
        let answer = match verb {
            "approve" => RemoteAnswer::Approve,
            "deny" => RemoteAnswer::Deny,
            _ => {
                return Verdict::Ignored(
                    "not a reply: expected `approve <token>` or `deny <token>`",
                );
            }
        };
        let Some(expected) = &self.token else {
            return Verdict::Ignored("this prompt was already answered");
        };
        if now >= self.expires {
            self.token = None;
            return Verdict::Ignored("the token for this prompt has expired");
        }
        if !constant_time_eq(expected.as_bytes(), presented.trim().as_bytes()) {
            return Verdict::Ignored(
                "the token does not match this prompt (meant for another prompt, reused, or \
                 guessed)",
            );
        }
        self.token = None;
        Verdict::Answer(answer)
    }
}

/// Compares without stopping at the first difference, so the time taken
/// says nothing about how much of a guess was right. The length is not
/// secret: every token is the same length.
fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.iter()
        .zip(right)
        .fold(0_u8, |difference, (a, b)| difference | (a ^ b))
        == 0
}

type ByteStream = Pin<Box<dyn Stream<Item = reqwest::Result<Bytes>> + Send>>;

/// The subscription's body, one line at a time.
struct Replies {
    stream: ByteStream,
    buffer: Vec<u8>,
}

impl Replies {
    fn new(stream: impl Stream<Item = reqwest::Result<Bytes>> + Send + 'static) -> Self {
        Self {
            stream: Box::pin(stream),
            buffer: Vec::new(),
        }
    }

    /// The next line without its newline. `Closed` when the server ends the
    /// stream. Cancel-safe: a partial line stays in the buffer.
    async fn next_line(&mut self) -> Result<String, RemoteApprovalError> {
        loop {
            if let Some(end) = self.buffer.iter().position(|&byte| byte == b'\n') {
                let line: Vec<u8> = self.buffer.drain(..=end).collect();
                return Ok(String::from_utf8_lossy(&line[..end]).into_owned());
            }
            if self.buffer.len() > MAX_LINE_BYTES {
                return Err(RemoteApprovalError::Transport {
                    message: "the server sent an over-long line".into(),
                });
            }
            match self.stream.next().await {
                Some(Ok(chunk)) => self.buffer.extend_from_slice(&chunk),
                Some(Err(error)) => return Err(transport(error)),
                None => return Err(RemoteApprovalError::Closed),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOKEN: &str = "00112233445566778899aabbccddeeff";

    fn pending(ttl: Duration) -> (PendingApproval, Instant) {
        let now = Instant::now();
        (PendingApproval::new(TOKEN.to_string(), now + ttl), now)
    }

    #[test]
    fn the_right_token_answers_once() {
        let (mut pending, now) = pending(Duration::from_secs(60));
        assert_eq!(
            pending.judge(&format!("deny {TOKEN}"), now),
            Verdict::Answer(RemoteAnswer::Deny)
        );
        assert_eq!(
            pending.judge(&format!("approve {TOKEN}"), now),
            Verdict::Ignored("this prompt was already answered"),
            "a used token must not answer again"
        );
    }

    #[test]
    fn a_wrong_or_malformed_reply_is_ignored_and_the_prompt_stays_answerable() {
        let (mut pending, now) = pending(Duration::from_secs(60));
        for reply in [
            "approve ffffffffffffffffffffffffffffffff",
            "approve 00112233",
            "approve",
            "yes",
            &format!("approve-all {TOKEN}"),
            &format!("APPROVE {TOKEN}"),
        ] {
            assert!(
                matches!(pending.judge(reply, now), Verdict::Ignored(_)),
                "{reply:?} was accepted"
            );
        }
        assert_eq!(
            pending.judge(&format!("approve {TOKEN}\n"), now),
            Verdict::Answer(RemoteAnswer::Approve)
        );
    }

    #[test]
    fn an_expired_token_answers_nothing_even_when_it_is_right() {
        let (mut pending, now) = pending(Duration::from_secs(60));
        let later = now + Duration::from_secs(60);
        assert_eq!(
            pending.judge(&format!("approve {TOKEN}"), later),
            Verdict::Ignored("the token for this prompt has expired")
        );
        assert!(
            matches!(
                pending.judge(&format!("approve {TOKEN}"), now),
                Verdict::Ignored(_)
            ),
            "expiry discards the token for good"
        );
    }

    #[test]
    fn a_reason_for_ignoring_never_quotes_the_reply() {
        let (mut pending, now) = pending(Duration::from_secs(60));
        let Verdict::Ignored(reason) = pending.judge("approve 0011223344556677889900", now) else {
            panic!("accepted a wrong token");
        };
        assert!(!reason.contains("0011"), "{reason}");
    }

    #[test]
    fn tokens_are_128_bits_of_hex_and_differ() {
        let first = one_time_token().unwrap();
        let second = one_time_token().unwrap();
        assert_eq!(first.len(), TOKEN_BYTES * 2);
        assert!(first.chars().all(|c| c.is_ascii_hexdigit()), "{first}");
        assert_ne!(first, second);
    }

    #[test]
    fn constant_time_eq_compares_whole_values() {
        assert!(constant_time_eq(b"abcd", b"abcd"));
        assert!(!constant_time_eq(b"abcd", b"abce"));
        assert!(!constant_time_eq(b"abcd", b"abc"));
    }

    #[test]
    fn only_message_events_on_the_reply_topic_are_replies() {
        let topic = "replies";
        assert_eq!(
            reply_message(
                r#"{"event":"message","topic":"replies","message":"approve x"}"#,
                topic
            )
            .as_deref(),
            Some("approve x")
        );
        for line in [
            r#"{"event":"open","topic":"replies"}"#,
            r#"{"event":"keepalive","topic":"replies"}"#,
            r#"{"event":"message","topic":"other","message":"approve x"}"#,
            "not json",
        ] {
            assert_eq!(reply_message(line, topic), None, "{line}");
        }
    }

    #[tokio::test]
    async fn lines_are_split_across_chunks_and_a_closed_stream_is_reported() {
        let chunks: Vec<reqwest::Result<Bytes>> = vec![
            Ok(Bytes::from_static(b"{\"a\":")),
            Ok(Bytes::from_static(b"1}\n{\"b\":2}\n")),
        ];
        let mut replies = Replies::new(futures::stream::iter(chunks));
        assert_eq!(replies.next_line().await.unwrap(), "{\"a\":1}");
        assert_eq!(replies.next_line().await.unwrap(), "{\"b\":2}");
        assert_eq!(replies.next_line().await, Err(RemoteApprovalError::Closed));
    }
}
