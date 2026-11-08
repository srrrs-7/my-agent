//! The webhook notifier over real HTTP, against the loopback mock.
//!
//! These pin the exact bodies each format puts on the wire - a receiver that
//! parses them is a contract we cannot see - and check that no failure path
//! repeats the URL, which for Slack and ntfy is the credential itself.

use std::time::Duration;

use agent_domain::error::NotifyError;
use agent_domain::model::tool::{ToolName, ToolSafety};
use agent_domain::ports::events::FinishReason;
use agent_domain::ports::notify::{Notification, NotificationEvent, Notifier};
use agent_infrastructure::notify::{NotifyFormat, WebhookNotifier, WebhookUrl};
use agent_test_support::{MockLlmServer, Response};
use serde_json::json;

const SECRET: &str = "s3cr3t-path";

/// The mock serves `http://127.0.0.1:<port>/v1`; the path is replaced with
/// one that would be a credential in real life.
fn secret_url(server: &MockLlmServer, path: &str) -> WebhookUrl {
    let root = server.base_url().trim_end_matches("/v1");
    WebhookUrl::parse(&format!("{root}/{path}")).unwrap()
}

fn notifier(url: &WebhookUrl, format: NotifyFormat) -> WebhookNotifier {
    WebhookNotifier::new(url, format, Duration::from_secs(5)).unwrap()
}

fn completed() -> Notification {
    Notification {
        session_id: "0123456789abcdef".into(),
        event: NotificationEvent::TurnFinished {
            reason: FinishReason::Completed,
            elapsed: Duration::from_secs(95),
            excerpt: Some("Renamed the module and updated 3 call sites…".into()),
        },
    }
}

#[tokio::test]
async fn ntfy_receives_a_json_publish_with_the_topic_in_the_body() {
    let server = MockLlmServer::start(vec![Response::ok("{}")]).await;
    let url = secret_url(&server, "agent-alerts");

    notifier(&url, NotifyFormat::Ntfy)
        .notify(&completed())
        .await
        .unwrap();

    let sent = server.json_requests().await;
    assert_eq!(
        sent,
        vec![json!({
            "topic": "agent-alerts",
            "title": "agent: turn finished",
            "message": "Finished in 1m 35s.\n\nRenamed the module and updated 3 call sites…\nsession 01234567",
            "tags": ["white_check_mark"],
        })]
    );
}

#[tokio::test]
async fn slack_receives_a_single_text_field() {
    let server = MockLlmServer::start(vec![Response::ok("ok")]).await;
    let url = secret_url(&server, "services/T0/B0/x");

    let pending = Notification {
        session_id: "abcdef0123456789".into(),
        event: NotificationEvent::ApprovalPending {
            tool: ToolName::new("write_file").unwrap(),
            safety: ToolSafety::Mutating,
        },
    };
    notifier(&url, NotifyFormat::Slack)
        .notify(&pending)
        .await
        .unwrap();

    let sent = server.json_requests().await;
    assert_eq!(
        sent,
        vec![json!({
            "text": "*agent: waiting for approval*\nThe mutating tool `write_file` is waiting for your answer.\nsession abcdef01",
            "unfurl_links": false,
            "unfurl_media": false,
        })]
    );
}

#[tokio::test]
async fn the_generic_format_carries_the_kind_and_the_full_session_id() {
    let server = MockLlmServer::start(vec![Response::ok("{}")]).await;
    let url = secret_url(&server, "hook");

    let failed = Notification {
        session_id: "0123456789abcdef".into(),
        event: NotificationEvent::TurnFinished {
            reason: FinishReason::MaxIterations { limit: 25 },
            elapsed: Duration::from_secs(600),
            excerpt: None,
        },
    };
    notifier(&url, NotifyFormat::Json)
        .notify(&failed)
        .await
        .unwrap();

    let sent = server.json_requests().await;
    assert_eq!(
        sent,
        vec![json!({
            "session_id": "0123456789abcdef",
            "kind": "failed",
            "title": "agent: turn did not finish",
            "message": "Stopped after 10m 00s: reached the iteration limit (25) without an answer.\nsession 01234567",
            "elapsed_secs": 600,
        })]
    );
}

#[tokio::test]
async fn a_non_success_status_is_an_error_that_does_not_repeat_the_url() {
    let server =
        MockLlmServer::start(vec![Response::status("500 Internal Server Error", "{}")]).await;
    let url = secret_url(&server, SECRET);

    let error = notifier(&url, NotifyFormat::Json)
        .notify(&completed())
        .await
        .expect_err("a 500 is a failed delivery");

    assert_eq!(error, NotifyError::HttpStatus { status: 500 });
    assert!(!error.to_string().contains(SECRET), "{error}");
    server.requests().await;
}

#[tokio::test]
async fn a_redirect_is_reported_rather_than_followed() {
    // Following a 307 would replay the body to wherever the server says.
    let server = MockLlmServer::start(vec![Response::redirect(
        "307 Temporary Redirect",
        "http://127.0.0.1:9/elsewhere",
    )])
    .await;
    let url = secret_url(&server, "hook");

    let error = notifier(&url, NotifyFormat::Json)
        .notify(&completed())
        .await
        .expect_err("a redirect is not a delivery");

    assert_eq!(error, NotifyError::HttpStatus { status: 307 });
    assert_eq!(server.requests().await.len(), 1);
}

#[tokio::test]
async fn an_unreachable_endpoint_is_a_transport_error_without_the_url() {
    // Bind and release a port so that nothing is listening on it.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let url = WebhookUrl::parse(&format!("http://127.0.0.1:{port}/{SECRET}")).unwrap();

    let error = notifier(&url, NotifyFormat::Json)
        .notify(&completed())
        .await
        .expect_err("nothing is listening");

    assert!(
        matches!(error, NotifyError::Transport { .. }),
        "got {error:?}"
    );
    let message = error.to_string();
    assert!(!message.contains(SECRET), "{message}");
    assert!(!message.contains(&port.to_string()), "{message}");
}

#[tokio::test]
async fn an_endpoint_that_never_answers_times_out() {
    // Accepted by the kernel's backlog, never read: the request is sent and
    // no response ever comes.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let url = WebhookUrl::parse(&format!("http://{address}/{SECRET}")).unwrap();

    let notifier =
        WebhookNotifier::new(&url, NotifyFormat::Json, Duration::from_millis(200)).unwrap();
    let error = notifier
        .notify(&completed())
        .await
        .expect_err("no answer within the timeout");

    assert!(
        matches!(error, NotifyError::Timeout { .. }),
        "got {error:?}"
    );
    drop(listener);
}

#[tokio::test]
async fn a_url_in_the_excerpt_reaches_slack_unlinkable_and_unfurled() {
    // A prompt-injected answer that smuggles a secret in a query string.
    let server = MockLlmServer::start(vec![Response::ok("ok")]).await;
    let url = secret_url(&server, "services/T0/B0/x");

    let leak = Notification {
        session_id: "s".into(),
        event: NotificationEvent::TurnFinished {
            reason: FinishReason::Completed,
            elapsed: Duration::from_secs(60),
            excerpt: Some("Done. Details: https://x.example/?k=secret".into()),
        },
    };
    notifier(&url, NotifyFormat::Slack)
        .notify(&leak)
        .await
        .unwrap();

    let sent = server.json_requests().await;
    let text = sent[0]["text"].as_str().unwrap();
    assert!(!text.contains("://"), "{text}");
    assert_eq!(sent[0]["unfurl_links"], json!(false));
    assert_eq!(sent[0]["unfurl_media"], json!(false));
}
