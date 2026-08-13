//! LLM adapters and the composable pieces around them.
//!
//! ```text
//!   Arc<dyn LlmProvider>
//!     = RetryingProvider(          <- backoff on transient failures
//!         RoutingLlmProvider(      <- picks a provider per request
//!           { "local": OpenAiCompatibleProvider,
//!             "cloud": AnthropicProvider }))
//! ```
//!
//! Each layer is an [`agent_domain::ports::llm::LlmProvider`] in its own right,
//! so the stack can be assembled, reordered or reduced to a single client
//! without any of the callers noticing.

pub mod anthropic;
pub mod factory;
pub mod openai;
pub mod retry;
pub mod routing;

pub use anthropic::AnthropicProvider;
pub use factory::build_provider;
pub use openai::OpenAiCompatibleProvider;
pub use retry::RetryingProvider;
pub use routing::{ModelPrefixRouter, RoutingLlmProvider, StaticRouter};

use agent_domain::error::LlmError;
use agent_domain::text;

/// How much of an unrecognised error body is kept. Enough to identify a proxy
/// or a gateway page, short enough not to flood the terminal.
const MAX_ERROR_BODY_BYTES: usize = 500;

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
