use std::sync::Arc;

use agent_domain::error::{FetchError, ToolError};
use agent_domain::model::tool::{ToolDefinition, ToolName, ToolOutcome, ToolSafety};
use agent_domain::ports::tool::Tool;
use agent_domain::ports::web::WebFetcher;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::tools::util::parse_arguments;

/// Fetches a URL and returns its text to the model.
///
/// All network policy (schemes, private-address blocking, redirects, size and
/// time limits) lives behind the [`WebFetcher`] port; this tool's job is to
/// translate between the model and that port. The fetched text reaches the
/// model exclusively as this tool's result - it is data, never instructions.
pub struct WebFetchTool {
    fetcher: Arc<dyn WebFetcher>,
}

#[derive(Debug, Deserialize)]
struct Input {
    url: String,
}

impl WebFetchTool {
    pub fn new(fetcher: Arc<dyn WebFetcher>) -> Self {
        Self { fetcher }
    }

    fn name() -> ToolName {
        ToolName::new("web_fetch").expect("static tool name is valid")
    }
}

#[async_trait]
impl Tool for WebFetchTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: Self::name(),
            description: "Fetch a public http(s) URL and return its content as text.\n\
                          HTML is reduced to prose. Private and internal addresses are \
                          refused. Long pages are truncated - fetch a more specific URL \
                          for more."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "url": {
                        "type": "string",
                        "description": "Absolute http(s) URL, e.g. `https://docs.rs/serde`."
                    }
                },
                "required": ["url"],
                "additionalProperties": false
            }),
            safety: ToolSafety::Network,
        }
    }

    async fn execute(&self, arguments: Value) -> Result<ToolOutcome, ToolError> {
        let name = Self::name();
        let input: Input = parse_arguments(&name, arguments)?;

        let page = self
            .fetcher
            .fetch(&input.url)
            .await
            .map_err(|error| match error {
                // The model's URL was unacceptable: tell it why so it can choose
                // another one instead of retrying the same request.
                FetchError::InvalidUrl { .. } | FetchError::Blocked { .. } => {
                    ToolError::invalid_input(&name, error.to_string())
                }
                other => ToolError::execution(&name, other.to_string()),
            })?;

        let mut content = format!("Fetched {} (HTTP {})\n\n", page.final_url, page.status);
        content.push_str(&page.text);
        if page.truncated {
            content.push_str(
                "\n\n[content truncated at the size limit - fetch a more specific URL for more]",
            );
        }

        let summary = format!("{} ({} bytes)", page.final_url, page.text.len());
        Ok(ToolOutcome::new(content).with_summary(summary))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_domain::ports::web::FetchedContent;

    struct StubFetcher(Result<FetchedContent, FetchError>);

    #[async_trait]
    impl WebFetcher for StubFetcher {
        async fn fetch(&self, _url: &str) -> Result<FetchedContent, FetchError> {
            self.0.clone()
        }
    }

    fn page(text: &str, truncated: bool) -> FetchedContent {
        FetchedContent {
            final_url: "https://example.com/docs".into(),
            status: 200,
            content_type: Some("text/html".into()),
            text: text.into(),
            truncated,
        }
    }

    #[tokio::test]
    async fn returns_the_page_text_with_provenance() {
        let tool = WebFetchTool::new(Arc::new(StubFetcher(Ok(page("Hello docs.", false)))));
        let outcome = tool
            .execute(json!({"url": "https://example.com/docs"}))
            .await
            .unwrap();

        assert!(
            outcome
                .content
                .starts_with("Fetched https://example.com/docs (HTTP 200)")
        );
        assert!(outcome.content.contains("Hello docs."));
        assert!(!outcome.content.contains("truncated"));
    }

    #[tokio::test]
    async fn marks_truncated_content() {
        let tool = WebFetchTool::new(Arc::new(StubFetcher(Ok(page("Partial", true)))));
        let outcome = tool
            .execute(json!({"url": "https://example.com"}))
            .await
            .unwrap();
        assert!(outcome.content.contains("[content truncated"));
    }

    #[tokio::test]
    async fn a_blocked_url_is_the_models_mistake() {
        let tool = WebFetchTool::new(Arc::new(StubFetcher(Err(FetchError::Blocked {
            url: "http://169.254.169.254/".into(),
            reason: "the address is link-local".into(),
        }))));
        let error = tool
            .execute(json!({"url": "http://169.254.169.254/"}))
            .await
            .unwrap_err();
        assert!(
            matches!(error, ToolError::InvalidInput { .. }),
            "blocked URLs must read as invalid input so the model picks another: {error}"
        );
    }

    #[tokio::test]
    async fn a_transport_failure_is_an_execution_error() {
        let tool = WebFetchTool::new(Arc::new(StubFetcher(Err(FetchError::Timeout {
            url: "https://example.com".into(),
            seconds: 30,
        }))));
        let error = tool
            .execute(json!({"url": "https://example.com"}))
            .await
            .unwrap_err();
        assert!(matches!(error, ToolError::Execution { .. }), "{error}");
    }

    #[test]
    fn the_tool_is_classified_as_network() {
        let tool = WebFetchTool::new(Arc::new(StubFetcher(Ok(page("", false)))));
        let definition = tool.definition();
        assert_eq!(definition.safety, ToolSafety::Network);
        assert!(
            !definition.safety.is_read_only(),
            "network tools must never auto-run under the read-only policy"
        );
    }
}
