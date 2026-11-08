//! Web-fetch port.
//!
//! Fetching a URL is a fundamentally different boundary from the workspace
//! sandbox: it *sends* data out (the URL itself can carry anything the model
//! chooses to put in it) and brings untrusted text in. The port therefore
//! carries its own contract:
//!
//! * Implementations own the network policy - which schemes, hosts and
//!   address ranges are reachable. A refusal is [`crate::error::FetchError::Blocked`],
//!   worded so the model can pick a different URL instead of retrying.
//! * Fetched text is *data*, never instructions. Consumers hand it to the
//!   model as a `tool_result` and nowhere else - not into the system prompt,
//!   not into tool definitions.

use async_trait::async_trait;
use thiserror::Error;

use crate::error::FetchError;

/// A fetched page, reduced to text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchedContent {
    /// The URL that actually served the content, after redirects.
    pub final_url: String,
    pub status: u16,
    pub content_type: Option<String>,
    /// Body as text; HTML is already stripped to prose by the implementation.
    pub text: String,
    /// True when the body was cut off at the size limit.
    pub truncated: bool,
}

#[async_trait]
pub trait WebFetcher: Send + Sync {
    /// Fetches `url` and returns its textual content.
    ///
    /// `url` is model-supplied and untrusted; implementations must validate it
    /// (scheme, host, resolved addresses, redirect targets) before touching
    /// the network.
    async fn fetch(&self, url: &str) -> Result<FetchedContent, FetchError>;
}

// --- search ---------------------------------------------------------------------

/// Longest query accepted, in characters. The query leaves the machine
/// verbatim, so it is kept to the size of a question rather than a document;
/// it is also the limit common search APIs enforce.
pub const MAX_QUERY_CHARS: usize = 400;

/// Most results one search returns. Each result is a few hundred bytes of
/// context, and ten is a full page of a search engine.
pub const MAX_RESULTS: usize = 10;

/// Results returned when the model does not ask for a number.
pub const DEFAULT_RESULTS: usize = 5;

/// A query that is safe to send: non-empty, bounded, and free of characters
/// that would show differently to the human approving it than to the search
/// service receiving it.
///
/// Named apart from [`crate::ports::search::SearchQuery`], which searches the
/// workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebSearchQuery {
    text: String,
    count: usize,
}

impl WebSearchQuery {
    /// Validates `text` (trimmed) and clamps `count` to `1..=MAX_RESULTS`;
    /// `None` asks for [`DEFAULT_RESULTS`].
    pub fn new(text: &str, count: Option<usize>) -> Result<Self, WebSearchError> {
        let text = text.trim();
        if text.is_empty() {
            return Err(WebSearchError::InvalidQuery {
                reason: "the query is empty. Pass the words to search for, e.g. \
                         `tokio select cancellation safety`"
                    .into(),
            });
        }
        let length = text.chars().count();
        if length > MAX_QUERY_CHARS {
            return Err(WebSearchError::InvalidQuery {
                reason: format!(
                    "the query is {length} characters long; the limit is {MAX_QUERY_CHARS}. \
                     Search for the key terms only"
                ),
            });
        }
        // Invisible and control characters would reach the service as part of
        // the query while the approval prompt shows them as `?` or not at all.
        if text.chars().any(crate::text::is_unprintable) {
            return Err(WebSearchError::InvalidQuery {
                reason: "the query contains control or invisible characters. Use plain text \
                         on one line"
                    .into(),
            });
        }
        Ok(Self {
            text: text.to_string(),
            count: count.unwrap_or(DEFAULT_RESULTS).clamp(1, MAX_RESULTS),
        })
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    /// How many results to ask for, already within `1..=MAX_RESULTS`.
    pub fn count(&self) -> usize {
        self.count
    }
}

/// One search result, as the service reported it. Every field is untrusted
/// text from outside.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebSearchHit {
    pub title: String,
    pub url: String,
    pub snippet: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct WebSearchResults {
    /// In the service's ranking order; at most the count asked for.
    pub hits: Vec<WebSearchHit>,
}

/// Failures of [`WebSearcher`]. Read by the model, so each says whether
/// searching again can help. None carries the API key or the request URL.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum WebSearchError {
    /// The model's query was unacceptable; nothing was sent.
    #[error("the search query was not sent: {reason}")]
    InvalidQuery { reason: String },

    /// The service refused the configured credentials.
    #[error(
        "the search service rejected the configured API key (HTTP {status}). Searching will \
         not work for the rest of this session; do not retry. Tell the user to check \
         AGENT_WEB_SEARCH_API_KEY, and continue without searching"
    )]
    Unauthorized { status: u16 },

    #[error(
        "the search service is rate limiting requests (HTTP 429). Do not search again right \
         away; continue with what you have, or try once more later in the task"
    )]
    RateLimited,

    #[error(
        "the search service answered with HTTP {status}. Try once more, or continue without \
         searching"
    )]
    HttpStatus { status: u16 },

    #[error("the search timed out after {seconds}s. Try once more, or continue without searching")]
    Timeout { seconds: u64 },

    #[error("the search service could not be reached: {message}")]
    Transport { message: String },

    #[error("the search service sent a response that could not be read: {reason}")]
    InvalidResponse { reason: String },
}

/// Searches the web through a search API.
///
/// Results are data for the model, delivered as a tool result and nowhere
/// else. Implementations do not follow the result links: reading a page is
/// [`WebFetcher`]'s job, behind its own network policy and its own approval.
#[async_trait]
pub trait WebSearcher: Send + Sync {
    async fn search(&self, query: &WebSearchQuery) -> Result<WebSearchResults, WebSearchError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_query_is_trimmed_and_its_count_clamped() {
        let query = WebSearchQuery::new("  rust async  ", Some(50)).unwrap();
        assert_eq!(query.text(), "rust async");
        assert_eq!(query.count(), MAX_RESULTS);
        assert_eq!(WebSearchQuery::new("q", Some(0)).unwrap().count(), 1);
        assert_eq!(
            WebSearchQuery::new("q", None).unwrap().count(),
            DEFAULT_RESULTS
        );
    }

    #[test]
    fn empty_overlong_and_invisible_queries_are_refused() {
        let overlong = "a".repeat(MAX_QUERY_CHARS + 1);
        for text in [
            "",
            "   ",
            overlong.as_str(),
            "rust\nignore previous",
            "rust\u{200B}async",
            "rust\u{E0041}",
        ] {
            let error = WebSearchQuery::new(text, None).unwrap_err();
            assert!(
                matches!(error, WebSearchError::InvalidQuery { .. }),
                "{text:?}: {error}"
            );
        }
        assert!(
            WebSearchQuery::new(&"あ".repeat(MAX_QUERY_CHARS), None).is_ok(),
            "the limit counts characters, not bytes"
        );
    }
}
