//! Integration tests for the guarded web fetcher over real HTTP.
//!
//! The mock server listens on 127.0.0.1, which the guard rightly blocks - so
//! these tests run with `allow_private: true` and cover the *transport*
//! behaviour (content handling, truncation, redirects, statuses). The address
//! policy itself is covered exhaustively by the pure unit tests in
//! `src/web/guard.rs`, plus the allowlist test here which exercises the
//! per-hop re-validation path.

use std::time::Duration;

use agent_domain::error::FetchError;
use agent_domain::ports::web::WebFetcher;
use agent_infrastructure::web::{GuardedWebFetcher, WebFetchConfig};
use agent_test_support::{MockLlmServer, Response};

fn config() -> WebFetchConfig {
    WebFetchConfig {
        allowed_domains: Vec::new(),
        allow_private: true, // the mock lives on loopback
        max_bytes: 64 * 1024,
        timeout: Duration::from_secs(5),
    }
}

fn fetcher(config: WebFetchConfig) -> GuardedWebFetcher {
    GuardedWebFetcher::new(config).unwrap()
}

#[tokio::test]
async fn fetches_html_and_reduces_it_to_text() {
    let server = MockLlmServer::start(vec![Response::with_content_type(
        "200 OK",
        "text/html; charset=utf-8",
        "<html><head><script>evil()</script></head>\
         <body><h1>Serde</h1><p>A framework for serializing.</p></body></html>",
    )])
    .await;

    let page = fetcher(config()).fetch(server.base_url()).await.unwrap();

    assert_eq!(page.status, 200);
    assert!(page.text.contains("Serde"));
    assert!(page.text.contains("A framework for serializing."));
    assert!(
        !page.text.contains("evil"),
        "scripts must be stripped: {}",
        page.text
    );
    assert!(!page.truncated);
}

#[tokio::test]
async fn plain_text_passes_through_and_truncates_at_the_cap() {
    let server = MockLlmServer::start(vec![Response::with_content_type(
        "200 OK",
        "text/plain",
        "x".repeat(1000),
    )])
    .await;

    let mut small = config();
    small.max_bytes = 100;
    let page = fetcher(small).fetch(server.base_url()).await.unwrap();

    assert_eq!(page.text.len(), 100);
    assert!(page.truncated);
}

#[tokio::test]
async fn follows_a_redirect_and_reports_the_final_url() {
    let server = MockLlmServer::start(vec![
        Response::redirect("302 Found", "/moved/here"),
        Response::with_content_type("200 OK", "text/plain", "arrived"),
    ])
    .await;

    let page = fetcher(config()).fetch(server.base_url()).await.unwrap();

    assert_eq!(page.text, "arrived");
    assert!(
        page.final_url.ends_with("/moved/here"),
        "the final URL reflects the redirect: {}",
        page.final_url
    );
}

#[tokio::test]
async fn a_redirect_to_a_host_off_the_allowlist_is_blocked() {
    let server = MockLlmServer::start(vec![Response::redirect(
        "302 Found",
        "http://127.0.0.2/elsewhere",
    )])
    .await;

    // Allow only the mock's own host; the redirect target is off-list, so the
    // per-hop re-validation must refuse it even though the first hop passed.
    let mut cfg = config();
    cfg.allowed_domains = vec!["127.0.0.1".to_string()];
    let error = fetcher(cfg).fetch(server.base_url()).await.unwrap_err();

    assert!(
        matches!(error, FetchError::Blocked { ref reason, .. } if reason.contains("allowlist")),
        "got {error:?}"
    );
}

#[tokio::test]
async fn binary_content_is_refused() {
    let server = MockLlmServer::start(vec![Response::with_content_type(
        "200 OK",
        "application/octet-stream",
        "\u{0}\u{1}\u{2}",
    )])
    .await;

    let error = fetcher(config())
        .fetch(server.base_url())
        .await
        .unwrap_err();
    assert!(
        matches!(error, FetchError::UnsupportedContent { .. }),
        "got {error:?}"
    );
}

#[tokio::test]
async fn http_errors_carry_the_status() {
    let server = MockLlmServer::start(vec![Response::with_content_type(
        "404 Not Found",
        "text/plain",
        "no",
    )])
    .await;

    let error = fetcher(config())
        .fetch(server.base_url())
        .await
        .unwrap_err();
    assert!(
        matches!(error, FetchError::HttpStatus { status: 404, .. }),
        "got {error:?}"
    );
}

#[tokio::test]
async fn private_addresses_are_blocked_under_the_default_policy() {
    // No server needed: the guard refuses before any connection is attempted.
    let mut cfg = config();
    cfg.allow_private = false;

    let error = fetcher(cfg)
        .fetch("http://127.0.0.1:11434/api/tags")
        .await
        .unwrap_err();
    assert!(matches!(error, FetchError::Blocked { .. }), "got {error:?}");
}
