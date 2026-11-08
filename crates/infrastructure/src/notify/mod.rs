//! Push notifications over a webhook.
//!
//! [`WebhookNotifier`] implements the [`agent_domain::ports::notify::Notifier`]
//! port for three wire formats: ntfy's JSON publish, a Slack incoming webhook,
//! and a plain JSON body for anything else.
//!
//! Two decisions that are easy to undo by accident:
//!
//! * **The URL is a secret.** A Slack webhook URL or a public ntfy topic is
//!   the whole credential - whoever knows it can post, and for ntfy, read.
//!   [`WebhookUrl`] hides everything past the host in `Debug`, and errors are
//!   built from `reqwest::Error::without_url`.
//! * **The guarded web fetcher is not used.** Its SSRF guard exists because
//!   the *model* chooses those URLs. This one is chosen by the operator, and a
//!   self-hosted ntfy on the LAN or on localhost is the common case the guard
//!   would refuse.

mod webhook;

use std::fmt;
use std::str::FromStr;
use std::time::Duration;

use url::Url;

pub use webhook::WebhookNotifier;

/// Longest a single delivery may take before it is abandoned. Short, because
/// `agent run` waits for the last one before exiting.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotifyFormat {
    /// ntfy's JSON publish: `POST /` with the topic in the body.
    Ntfy,
    /// A Slack incoming webhook: `{"text": ...}`.
    Slack,
    /// A neutral JSON object for anything that is neither.
    Json,
}

impl NotifyFormat {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ntfy => "ntfy",
            Self::Slack => "slack",
            Self::Json => "json",
        }
    }

    /// The format a URL most likely wants when the operator did not say.
    ///
    /// Only the public ntfy server is recognised. Guessing from anything
    /// looser (a host that merely contains "ntfy") would silently change the
    /// body sent to someone's own endpoint.
    pub fn infer(url: &WebhookUrl) -> Self {
        if url.0.host_str() == Some("ntfy.sh") {
            Self::Ntfy
        } else {
            Self::Json
        }
    }
}

impl FromStr for NotifyFormat {
    type Err = String;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        match raw.to_ascii_lowercase().as_str() {
            "ntfy" => Ok(Self::Ntfy),
            "slack" => Ok(Self::Slack),
            "json" => Ok(Self::Json),
            _ => Err("expected one of: ntfy, slack, json".to_string()),
        }
    }
}

/// A webhook destination, treated as a credential.
///
/// There is deliberately no `Display` and no accessor for the full URL outside
/// this crate: the only things that can be done with it from the outside are
/// to show its redacted form and to hand it to [`WebhookNotifier`].
#[derive(Clone, PartialEq, Eq)]
pub struct WebhookUrl(Url);

impl WebhookUrl {
    /// Accepts absolute `http` / `https` URLs with a host.
    ///
    /// The error never repeats the input, because the input is the secret.
    pub fn parse(raw: &str) -> Result<Self, &'static str> {
        let url = Url::parse(raw).map_err(|_| "not an absolute URL")?;
        if !matches!(url.scheme(), "http" | "https") {
            return Err("only http:// and https:// URLs are supported");
        }
        if url.host_str().is_none_or(str::is_empty) {
            return Err("the URL has no host");
        }
        Ok(Self(url))
    }

    /// `scheme://host/…` - enough to tell which service is configured, nothing
    /// that grants access to it. The port is dropped along with the path:
    /// it is rarely what someone checking `doctor` needs to see.
    pub fn redacted(&self) -> String {
        format!(
            "{}://{}/…",
            self.0.scheme(),
            self.0.host_str().unwrap_or_default()
        )
    }

    /// The parts of the URL that grant access, for redaction: the URL itself,
    /// its path and query wherever they appear without it, any password, and
    /// for ntfy the topic on its own - knowing the topic is enough to read it.
    ///
    /// Short parts are dropped later by the redactor's length threshold.
    pub(crate) fn secrets(&self, format: NotifyFormat) -> Vec<String> {
        let mut secrets = vec![self.0.as_str().to_string()];
        let path = self.0.path().trim_matches('/');
        if !path.is_empty() {
            secrets.push(path.to_string());
        }
        secrets.extend(self.0.query().map(str::to_string));
        secrets.extend(self.0.password().map(str::to_string));
        if format == NotifyFormat::Ntfy {
            secrets.extend(
                endpoint(self, format)
                    .ok()
                    .and_then(|endpoint| endpoint.topic),
            );
        }
        secrets
    }
}

impl fmt::Debug for WebhookUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("WebhookUrl").field(&self.redacted()).finish()
    }
}

/// Where a request actually goes, and the ntfy topic split out of the URL.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct Endpoint {
    pub(crate) url: Url,
    pub(crate) topic: Option<String>,
}

/// Resolves the URL the operator configured into the URL a request is sent to.
///
/// Only ntfy differs: `https://host/topic` is published to by `POST`ing
/// `{"topic": "topic", ...}` to `https://host/`. The topic goes in the body
/// rather than in an `X-Title`-style header scheme so that nothing model
/// written ever ends up in a header.
///
/// Errors are phrased for the operator and never contain the URL. Config
/// calls this at startup so a bad URL fails there and not on first delivery.
pub(crate) fn endpoint(url: &WebhookUrl, format: NotifyFormat) -> Result<Endpoint, String> {
    if format != NotifyFormat::Ntfy {
        return Ok(Endpoint {
            url: url.0.clone(),
            topic: None,
        });
    }

    let mut segments: Vec<&str> = url
        .0
        .path_segments()
        .map(|segments| segments.filter(|segment| !segment.is_empty()).collect())
        .unwrap_or_default();
    let topic = segments.pop().ok_or_else(|| {
        "ntfy needs the topic as the last path segment, e.g. https://ntfy.sh/<topic>".to_string()
    })?;
    // ntfy's own rule. Checked here so a typo fails at startup rather than as
    // a 400 on the first notification, which nobody would be watching for.
    if topic.len() > 64
        || !topic
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(
            "the ntfy topic (the last path segment) may only contain [A-Za-z0-9_-], \
             up to 64 characters"
                .to_string(),
        );
    }

    let mut root = url.0.clone();
    let base: String = segments
        .iter()
        .map(|segment| format!("/{segment}"))
        .collect();
    root.set_path(&format!("{base}/"));
    root.set_fragment(None);

    Ok(Endpoint {
        url: root,
        topic: Some(topic.to_string()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(raw: &str) -> WebhookUrl {
        WebhookUrl::parse(raw).unwrap()
    }

    #[test]
    fn an_ntfy_url_is_split_into_the_server_root_and_the_topic() {
        let endpoint = endpoint(&url("https://ntfy.sh/my-agent_42"), NotifyFormat::Ntfy).unwrap();
        assert_eq!(endpoint.url.as_str(), "https://ntfy.sh/");
        assert_eq!(endpoint.topic.as_deref(), Some("my-agent_42"));
    }

    #[test]
    fn a_self_hosted_ntfy_under_a_subpath_keeps_its_prefix() {
        let endpoint = endpoint(
            &url("http://192.168.1.10:8080/ntfy/alerts/"),
            NotifyFormat::Ntfy,
        )
        .unwrap();
        assert_eq!(endpoint.url.as_str(), "http://192.168.1.10:8080/ntfy/");
        assert_eq!(endpoint.topic.as_deref(), Some("alerts"));
    }

    #[test]
    fn ntfy_without_a_usable_topic_is_rejected_without_echoing_the_url() {
        let error = endpoint(&url("https://ntfy.sh/"), NotifyFormat::Ntfy)
            .err()
            .expect("no topic");
        assert!(error.contains("topic"), "{error}");

        let error = endpoint(&url("https://ntfy.sh/bad.topic"), NotifyFormat::Ntfy)
            .err()
            .expect("invalid topic");
        assert!(!error.contains("bad.topic"), "{error}");
    }

    #[test]
    fn other_formats_post_to_the_url_as_given() {
        let endpoint = endpoint(
            &url("https://hooks.slack.com/services/T/B/secret"),
            NotifyFormat::Slack,
        )
        .unwrap();
        assert_eq!(
            endpoint.url.as_str(),
            "https://hooks.slack.com/services/T/B/secret"
        );
        assert_eq!(endpoint.topic, None);
    }

    #[test]
    fn only_http_urls_with_a_host_are_accepted() {
        assert!(WebhookUrl::parse("ntfy.sh/topic").is_err());
        assert!(WebhookUrl::parse("file:///etc/passwd").is_err());
        assert!(WebhookUrl::parse("ftp://example.com/x").is_err());
        assert!(WebhookUrl::parse("http://localhost:8080/topic").is_ok());
    }

    #[test]
    fn debug_and_redacted_show_the_host_but_not_the_path() {
        let secret = url("https://user:pw@hooks.slack.com:8443/services/T0/B0/XYZSECRET?x=1");
        let shown = format!("{secret:?} {}", secret.redacted());
        assert!(shown.contains("hooks.slack.com"), "{shown}");
        for hidden in ["XYZSECRET", "services", "user", "pw", "x=1"] {
            assert!(!shown.contains(hidden), "`{hidden}` leaked: {shown}");
        }
    }

    #[test]
    fn the_secret_parts_of_a_webhook_url_cover_its_path_and_the_ntfy_topic() {
        let slack = url("https://hooks.slack.com/services/T000/B000/XXXXSECRET");
        let secrets = slack.secrets(NotifyFormat::Slack);
        assert!(
            secrets.contains(&"services/T000/B000/XXXXSECRET".to_string()),
            "{secrets:?}"
        );
        assert!(
            !secrets.iter().any(|secret| secret == "hooks.slack.com"),
            "the host is not a secret: {secrets:?}"
        );

        let ntfy = url("https://ntfy.sh/my-private-topic-42");
        let secrets = ntfy.secrets(NotifyFormat::Ntfy);
        assert!(
            secrets.contains(&"my-private-topic-42".to_string()),
            "{secrets:?}"
        );
        assert!(secrets.contains(&"https://ntfy.sh/my-private-topic-42".to_string()));
    }

    #[test]
    fn only_the_public_ntfy_server_is_recognised_by_its_host() {
        assert_eq!(
            NotifyFormat::infer(&url("https://ntfy.sh/topic")),
            NotifyFormat::Ntfy
        );
        assert_eq!(
            NotifyFormat::infer(&url("https://ntfy.example.com/topic")),
            NotifyFormat::Json
        );
    }

    #[test]
    fn format_names_are_case_insensitive_and_typos_are_errors() {
        assert_eq!("Slack".parse::<NotifyFormat>(), Ok(NotifyFormat::Slack));
        assert!("slak".parse::<NotifyFormat>().is_err());
    }
}
