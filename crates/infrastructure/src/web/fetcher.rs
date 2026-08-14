//! The guarded web fetcher.
//!
//! Every request runs the full gauntlet: lexical URL checks, DNS-resolution
//! checks, an optional operator allowlist - and again for every redirect hop,
//! because a permitted host can redirect anywhere. Redirects are therefore
//! never followed by the HTTP client itself.

use std::net::IpAddr;
use std::time::Duration;

use agent_domain::error::FetchError;
use agent_domain::ports::web::{FetchedContent, WebFetcher};
use async_trait::async_trait;
use futures::StreamExt as _;
use tracing::debug;
use url::Url;

use super::html::html_to_text;
use crate::net::guard::{DomainScope, HostPolicy, Rejection, RejectionKind};

/// How many redirect hops are followed before giving up. Fixed: a legitimate
/// documentation URL does not need more, and every hop is re-validated.
const MAX_REDIRECTS: usize = 5;

#[derive(Debug, Clone)]
pub struct WebFetchConfig {
    /// Domain suffixes the operator allows (`docs.rs` also admits
    /// `foo.docs.rs`). Empty = any host that passes the address policy.
    pub allowed_domains: Vec<String>,
    /// Lifts the private/internal-address policy (intranet use, tests).
    /// Scheme and credential rules still apply.
    pub allow_private: bool,
    /// Bytes of body read before truncating.
    pub max_bytes: usize,
    pub timeout: Duration,
}

pub struct GuardedWebFetcher {
    client: reqwest::Client,
    policy: HostPolicy,
    config: WebFetchConfig,
}

/// Attaches the URL that a shared-guard rejection was about.
fn rejected(url: &str, rejection: Rejection) -> FetchError {
    match rejection.kind {
        RejectionKind::Malformed => FetchError::InvalidUrl {
            url: url.to_string(),
            reason: rejection.reason,
        },
        RejectionKind::Blocked => FetchError::Blocked {
            url: url.to_string(),
            reason: rejection.reason,
        },
    }
}

impl GuardedWebFetcher {
    pub fn new(config: WebFetchConfig) -> Result<Self, FetchError> {
        let client = reqwest::Client::builder()
            .timeout(config.timeout)
            // Redirects are validated and followed manually, one hop at a time.
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(concat!("my-agent/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|error| FetchError::Transport {
                url: String::new(),
                message: format!("cannot build the HTTP client: {error}"),
            })?;

        let policy = HostPolicy {
            allow_private: config.allow_private,
            // An unset allowlist opens onto the public internet here; the
            // command sandbox makes the opposite choice with the same type.
            scope: if config.allowed_domains.is_empty() {
                DomainScope::AnyPublic
            } else {
                DomainScope::Only(config.allowed_domains.clone())
            },
        };

        Ok(Self {
            client,
            policy,
            config,
        })
    }

    /// Full admission check for one URL (original or redirect target).
    async fn admit(&self, raw: &str) -> Result<Url, FetchError> {
        let url = self
            .policy
            .check_url(raw)
            .map_err(|rejection| rejected(raw, rejection))?;

        // Resolve and vet every address behind a hostname. IP literals were
        // already vetted lexically.
        if self.policy.resolves_before_connect() {
            if let Some(url::Host::Domain(name)) = url.host() {
                let port = url.port_or_known_default().unwrap_or(443);
                let addrs: Vec<IpAddr> = tokio::net::lookup_host((name, port))
                    .await
                    .map_err(|error| FetchError::Transport {
                        url: raw.to_string(),
                        message: format!("DNS resolution failed: {error}"),
                    })?
                    .map(|socket| socket.ip())
                    .collect();
                self.policy
                    .check_addrs(addrs)
                    .map_err(|rejection| rejected(raw, rejection))?;
            }
        }

        Ok(url)
    }

    async fn read_body(
        &self,
        response: reqwest::Response,
        url: &str,
    ) -> Result<(Vec<u8>, bool), FetchError> {
        let mut body: Vec<u8> = Vec::new();
        let mut truncated = false;
        let mut stream = response.bytes_stream();

        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|error| transport(url, self.config.timeout, error))?;
            if body.len() + chunk.len() > self.config.max_bytes {
                body.extend_from_slice(&chunk[..self.config.max_bytes - body.len()]);
                truncated = true;
                break;
            }
            body.extend_from_slice(&chunk);
        }
        Ok((body, truncated))
    }
}

#[async_trait]
impl WebFetcher for GuardedWebFetcher {
    async fn fetch(&self, url: &str) -> Result<FetchedContent, FetchError> {
        let mut current = self.admit(url).await?;

        for _hop in 0..=MAX_REDIRECTS {
            let response = self
                .client
                .get(current.clone())
                .send()
                .await
                .map_err(|error| transport(current.as_str(), self.config.timeout, error))?;

            let status = response.status();

            if status.is_redirection() {
                let location = response
                    .headers()
                    .get(reqwest::header::LOCATION)
                    .and_then(|value| value.to_str().ok())
                    .ok_or_else(|| FetchError::Transport {
                        url: current.to_string(),
                        message: format!("HTTP {status} without a Location header"),
                    })?;
                // Relative Locations resolve against the current URL; the
                // result goes through the full admission check like any other.
                let next = current
                    .join(location)
                    .map_err(|error| FetchError::InvalidUrl {
                        url: location.to_string(),
                        reason: error.to_string(),
                    })?;
                debug!(from = %current, to = %next, "following redirect");
                current = self.admit(next.as_str()).await?;
                continue;
            }

            if !status.is_success() {
                return Err(FetchError::HttpStatus {
                    url: current.to_string(),
                    status: status.as_u16(),
                });
            }

            let content_type = response
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok())
                .map(|value| value.to_string());

            let kind = classify(content_type.as_deref());
            if kind == ContentKind::Binary {
                return Err(FetchError::UnsupportedContent {
                    url: current.to_string(),
                    content_type: content_type.unwrap_or_else(|| "unknown".into()),
                });
            }

            let (body, truncated) = self.read_body(response, current.as_str()).await?;
            let raw_text = String::from_utf8_lossy(&body).into_owned();
            let text = match kind {
                ContentKind::Html => html_to_text(&raw_text),
                _ => raw_text,
            };

            return Ok(FetchedContent {
                final_url: current.to_string(),
                status: status.as_u16(),
                content_type,
                text,
                truncated,
            });
        }

        Err(FetchError::Blocked {
            url: url.to_string(),
            reason: format!("more than {MAX_REDIRECTS} redirects"),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ContentKind {
    Html,
    Text,
    Binary,
}

fn classify(content_type: Option<&str>) -> ContentKind {
    let Some(content_type) = content_type else {
        // No header: assume text, the lossy conversion defuses binary anyway.
        return ContentKind::Text;
    };
    let lower = content_type.to_ascii_lowercase();
    if lower.contains("html") {
        ContentKind::Html
    } else if lower.starts_with("text/")
        || lower.contains("json")
        || lower.contains("xml")
        || lower.contains("javascript")
        || lower.contains("charset")
    {
        ContentKind::Text
    } else {
        ContentKind::Binary
    }
}

fn transport(url: &str, timeout: Duration, error: reqwest::Error) -> FetchError {
    if error.is_timeout() {
        FetchError::Timeout {
            url: url.to_string(),
            seconds: timeout.as_secs(),
        }
    } else {
        FetchError::Transport {
            url: url.to_string(),
            message: error.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_types_classify_sensibly() {
        assert_eq!(
            classify(Some("text/html; charset=utf-8")),
            ContentKind::Html
        );
        assert_eq!(classify(Some("application/xhtml+xml")), ContentKind::Html);
        assert_eq!(classify(Some("text/plain")), ContentKind::Text);
        assert_eq!(classify(Some("application/json")), ContentKind::Text);
        assert_eq!(classify(None), ContentKind::Text);
        assert_eq!(classify(Some("image/png")), ContentKind::Binary);
        assert_eq!(
            classify(Some("application/octet-stream")),
            ContentKind::Binary
        );
    }
}
