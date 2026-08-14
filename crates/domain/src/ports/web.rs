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
