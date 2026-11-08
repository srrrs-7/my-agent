//! Web search through the Brave Search API.
//!
//! One request per search, to one fixed host: the endpoint is not a setting,
//! because whoever chooses it receives every query and the API key. Tests
//! substitute a loopback server through a private constructor.
//!
//! Brave's response nests the results under `web.results`, marks matched
//! terms in `description` with `<strong>`, and omits `web` entirely when
//! nothing matched. The parsing is lenient (`#[serde(default)]`) so a field
//! Brave drops or adds does not turn every search into an error.

use std::fmt;
use std::time::Duration;

use agent_domain::ports::web::{
    WebSearchError, WebSearchHit, WebSearchQuery, WebSearchResults, WebSearcher,
};
use async_trait::async_trait;
use serde::Deserialize;
use url::Url;

use super::html::html_to_text;

const BRAVE_ENDPOINT: &str = "https://api.search.brave.com/res/v1/web/search";

/// A search either answers in seconds or is not worth waiting for; the tool
/// timeout above it is a backstop, not the plan.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// The search API key. `Debug` never shows it, so the settings that hold it
/// can be printed and logged.
#[derive(Clone, PartialEq, Eq)]
pub struct SearchApiKey(String);

impl SearchApiKey {
    pub fn new(key: impl Into<String>) -> Self {
        Self(key.into())
    }

    /// The key itself - for the request header and the redactor, nothing else.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for SearchApiKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SearchApiKey(***)")
    }
}

pub struct BraveSearcher {
    client: reqwest::Client,
    endpoint: Url,
    key: SearchApiKey,
}

impl fmt::Debug for BraveSearcher {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BraveSearcher")
            .field("endpoint", &self.endpoint.as_str())
            .field("key", &self.key)
            .finish()
    }
}

impl BraveSearcher {
    pub fn new(key: SearchApiKey) -> Result<Self, WebSearchError> {
        Self::with_endpoint(key, BRAVE_ENDPOINT)
    }

    /// Private so that only this module's tests can point it elsewhere.
    fn with_endpoint(key: SearchApiKey, endpoint: &str) -> Result<Self, WebSearchError> {
        let endpoint = Url::parse(endpoint).map_err(|error| WebSearchError::Transport {
            message: format!("the search endpoint is not a URL: {error}"),
        })?;
        let client = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .connect_timeout(CONNECT_TIMEOUT)
            // The API answers directly; a redirect would carry the key
            // header to a host nobody chose.
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(concat!("my-agent/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|error| WebSearchError::Transport {
                message: format!("cannot build the HTTP client: {error}"),
            })?;
        Ok(Self {
            client,
            endpoint,
            key,
        })
    }

    fn request_url(&self, query: &WebSearchQuery) -> Url {
        let mut url = self.endpoint.clone();
        url.query_pairs_mut()
            .append_pair("q", query.text())
            .append_pair("count", &query.count().to_string());
        url
    }
}

#[async_trait]
impl WebSearcher for BraveSearcher {
    async fn search(&self, query: &WebSearchQuery) -> Result<WebSearchResults, WebSearchError> {
        let response = self
            .client
            .get(self.request_url(query))
            .header("Accept", "application/json")
            .header("X-Subscription-Token", self.key.expose())
            .send()
            .await
            .map_err(transport)?;

        let status = response.status().as_u16();
        match status {
            200..=299 => {}
            401 | 403 => return Err(WebSearchError::Unauthorized { status }),
            429 => return Err(WebSearchError::RateLimited),
            // The body is not quoted: nothing in it helps the model, and an
            // error page may echo the request back.
            _ => return Err(WebSearchError::HttpStatus { status }),
        }

        let body = response.bytes().await.map_err(transport)?;
        parse(&body, query.count())
    }
}

/// The request URL holds the query, which the model already has; it is
/// dropped anyway so that no transport error carries more than its cause.
fn transport(error: reqwest::Error) -> WebSearchError {
    if error.is_timeout() {
        WebSearchError::Timeout {
            seconds: REQUEST_TIMEOUT.as_secs(),
        }
    } else {
        WebSearchError::Transport {
            message: error.without_url().to_string(),
        }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct BraveResponse {
    web: BraveWeb,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct BraveWeb {
    results: Vec<BraveResult>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct BraveResult {
    title: String,
    url: String,
    description: String,
}

fn parse(body: &[u8], limit: usize) -> Result<WebSearchResults, WebSearchError> {
    let response: BraveResponse =
        serde_json::from_slice(body).map_err(|error| WebSearchError::InvalidResponse {
            reason: format!("not the expected JSON ({error})"),
        })?;

    let hits = response
        .web
        .results
        .into_iter()
        // A result without a link cannot be followed or cited.
        .filter(|result| !result.url.trim().is_empty())
        .take(limit)
        .map(|result| WebSearchHit {
            title: inline_text(&result.title),
            url: result.url.trim().to_string(),
            snippet: inline_text(&result.description),
        })
        .collect();
    Ok(WebSearchResults { hits })
}

/// Brave marks matches with `<strong>` and escapes entities; the model wants
/// the words, on one line.
fn inline_text(html: &str) -> String {
    html_to_text(html)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_test_support::{RecordingServer, Response};

    const KEY: &str = "brave-test-key-0123456789";

    fn searcher(server: &RecordingServer) -> BraveSearcher {
        let endpoint = format!("{}/res/v1/web/search", server.url());
        BraveSearcher::with_endpoint(SearchApiKey::new(KEY), &endpoint).unwrap()
    }

    fn query(text: &str, count: usize) -> WebSearchQuery {
        WebSearchQuery::new(text, Some(count)).unwrap()
    }

    #[tokio::test]
    async fn the_request_carries_the_query_the_count_and_the_token() {
        let server = RecordingServer::start(Response::ok(r#"{"web":{"results":[]}}"#)).await;

        searcher(&server)
            .search(&query("rust & tokio", 3))
            .await
            .unwrap();

        let received = server.received();
        assert_eq!(received.len(), 1, "{received:?}");
        let request = &received[0];
        assert_eq!(request.method, "GET");
        assert_eq!(request.path, "/res/v1/web/search?q=rust+%26+tokio&count=3");
        assert_eq!(request.header("x-subscription-token"), Some(KEY));
        assert_eq!(request.header("accept"), Some("application/json"));
    }

    #[tokio::test]
    async fn results_are_read_in_order_and_reduced_to_text() {
        let body = r#"{
            "type": "search",
            "web": {"results": [
                {"title": "Tokio &amp; friends", "url": "https://tokio.rs",
                 "description": "An <strong>async</strong> runtime.", "age": "2 days"},
                {"title": "Docs", "url": "https://docs.rs/tokio"}
            ]}
        }"#;
        let server = RecordingServer::start(Response::ok(body)).await;

        let results = searcher(&server).search(&query("tokio", 5)).await.unwrap();

        assert_eq!(
            results.hits,
            vec![
                WebSearchHit {
                    title: "Tokio & friends".into(),
                    url: "https://tokio.rs".into(),
                    snippet: "An async runtime.".into(),
                },
                WebSearchHit {
                    title: "Docs".into(),
                    url: "https://docs.rs/tokio".into(),
                    snippet: String::new(),
                },
            ]
        );
    }

    #[test]
    fn missing_sections_mean_no_results_and_linkless_results_are_dropped() {
        assert!(parse(b"{}", 5).unwrap().hits.is_empty());
        assert!(parse(br#"{"web":{}}"#, 5).unwrap().hits.is_empty());

        let results = parse(
            br#"{"web":{"results":[{"title":"no link"},{"url":"https://a.example"}]}}"#,
            5,
        )
        .unwrap();
        assert_eq!(results.hits.len(), 1);
        assert_eq!(results.hits[0].url, "https://a.example");
        assert_eq!(results.hits[0].title, "");
    }

    #[test]
    fn never_more_results_than_asked_for() {
        let body =
            r#"{"web":{"results":[{"url":"https://1"},{"url":"https://2"},{"url":"https://3"}]}}"#;
        assert_eq!(parse(body.as_bytes(), 2).unwrap().hits.len(), 2);
    }

    #[test]
    fn a_body_that_is_not_json_is_an_invalid_response() {
        let error = parse(b"<html>oops</html>", 5).unwrap_err();
        assert!(
            matches!(error, WebSearchError::InvalidResponse { .. }),
            "{error}"
        );
    }

    async fn error_for(status_line: &str) -> WebSearchError {
        let body = format!(r#"{{"error":"bad token {KEY}"}}"#);
        let server = RecordingServer::start(Response::status(status_line, body)).await;
        searcher(&server).search(&query("q", 1)).await.unwrap_err()
    }

    #[tokio::test]
    async fn a_rejected_key_tells_the_model_to_stop_without_showing_the_key() {
        for status_line in ["401 Unauthorized", "403 Forbidden"] {
            let error = error_for(status_line).await;
            assert!(
                matches!(error, WebSearchError::Unauthorized { .. }),
                "{error}"
            );
            let message = error.to_string();
            assert!(message.contains("do not retry"), "{message}");
            assert!(message.contains("AGENT_WEB_SEARCH_API_KEY"), "{message}");
            assert!(!message.contains(KEY), "the key leaked: {message}");
            assert!(!format!("{error:?}").contains(KEY), "{error:?}");
        }
    }

    #[tokio::test]
    async fn rate_limiting_says_not_to_search_again_right_away() {
        let error = error_for("429 Too Many Requests").await;
        assert_eq!(error, WebSearchError::RateLimited);
        assert!(!error.to_string().contains(KEY));
    }

    #[tokio::test]
    async fn another_status_is_reported_without_the_body() {
        let error = error_for("502 Bad Gateway").await;
        assert_eq!(error, WebSearchError::HttpStatus { status: 502 });
        assert!(!error.to_string().contains(KEY), "{error}");
    }

    #[tokio::test]
    async fn a_redirect_is_not_followed_with_the_key() {
        let server =
            RecordingServer::start(Response::redirect("302 Found", "http://127.0.0.1:9/")).await;
        let error = searcher(&server).search(&query("q", 1)).await.unwrap_err();
        assert_eq!(error, WebSearchError::HttpStatus { status: 302 });
        assert_eq!(server.received().len(), 1);
    }

    #[test]
    fn the_key_never_appears_in_debug_output() {
        let key = SearchApiKey::new(KEY);
        assert!(!format!("{key:?}").contains(KEY));
        let searcher = BraveSearcher::new(key).unwrap();
        assert!(!format!("{searcher:?}").contains(KEY), "{searcher:?}");
    }
}
