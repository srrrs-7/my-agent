//! HTTP plumbing shared by every LLM client.
//!
//! Vendors differ in payload shape and auth headers; everything else - client
//! construction, timeout mapping, status handling, `Retry-After`, error-body
//! extraction - is identical, so it lives here once. A fix to any of it (say,
//! honouring a new rate-limit header) lands in every provider at the same time.

use std::time::Duration;

use agent_domain::error::LlmError;
use agent_domain::text;

/// How much of an unrecognised error body is kept. Enough to identify a proxy
/// or a gateway page, short enough not to flood the terminal.
const MAX_ERROR_BODY_BYTES: usize = 500;

/// The one way an LLM adapter constructs its client.
pub(crate) fn build_client(timeout: Duration) -> Result<reqwest::Client, LlmError> {
    reqwest::Client::builder()
        .timeout(timeout)
        .user_agent(concat!("my-agent/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|error| LlmError::Configuration(error.to_string()))
}

/// Sends a fully-built request and returns the successful response body.
///
/// Transport failures become [`LlmError::Timeout`] / [`LlmError::Transport`]
/// (tagged with `base_url` so a wrong endpoint is diagnosable), and
/// non-success statuses go through [`map_http_failure`] so every client
/// reports the same error for the same condition.
pub(crate) async fn send(
    request: reqwest::RequestBuilder,
    base_url: &str,
    timeout: Duration,
) -> Result<String, LlmError> {
    let transport = |error: reqwest::Error| {
        if error.is_timeout() {
            LlmError::Timeout {
                seconds: timeout.as_secs(),
            }
        } else {
            LlmError::Transport(format!("{error} ({base_url})"))
        }
    };

    let response = request.send().await.map_err(transport)?;
    let status = response.status();
    let retry_after = response
        .headers()
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok());
    let text = response.text().await.map_err(transport)?;

    if !status.is_success() {
        return Err(map_http_failure(status.as_u16(), retry_after, &text));
    }
    Ok(text)
}

/// Shared HTTP status mapping so every client reports failures the same way.
pub(crate) fn map_http_failure(status: u16, retry_after_secs: Option<u64>, body: &str) -> LlmError {
    let message = extract_error_message(body);
    match status {
        401 | 403 => LlmError::Auth(message),
        429 => LlmError::RateLimited { retry_after_secs },
        _ => LlmError::Api { status, message },
    }
}

/// Both vendors nest the useful part of an error under `error.message`; fall
/// back to the raw body (clipped) when the shape is something else.
fn extract_error_message(body: &str) -> String {
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(body) {
        if let Some(message) = value.get("error").and_then(|error| error.get("message")) {
            if let Some(message) = message.as_str() {
                return message.to_string();
            }
        }
        if let Some(message) = value.get("message").and_then(|message| message.as_str()) {
            return message.to_string();
        }
    }
    let trimmed = body.trim();
    if trimmed.is_empty() {
        "(empty response body)".to_string()
    } else {
        text::clip(trimmed, MAX_ERROR_BODY_BYTES)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unwraps_nested_error_messages() {
        let body = r#"{"error":{"message":"model not found","type":"invalid_request_error"}}"#;
        assert!(matches!(
            map_http_failure(404, None, body),
            LlmError::Api { status: 404, message } if message == "model not found"
        ));
    }

    #[test]
    fn maps_auth_and_rate_limits() {
        assert!(matches!(
            map_http_failure(401, None, "{}"),
            LlmError::Auth(_)
        ));
        assert!(matches!(
            map_http_failure(429, Some(30), "{}"),
            LlmError::RateLimited {
                retry_after_secs: Some(30)
            }
        ));
    }

    #[test]
    fn falls_back_to_the_raw_body() {
        assert!(matches!(
            map_http_failure(502, None, "<html>bad gateway</html>"),
            LlmError::Api { message, .. } if message.contains("bad gateway")
        ));
    }
}
