//! Exercises the OpenAI-compatible client over a real socket.
//!
//! The unit tests in `llm::openai` cover payload mapping; this covers what only
//! an actual HTTP round-trip can: status handling and the exact JSON that goes
//! on the wire.

use std::time::Duration;

use agent_domain::error::LlmError;
use agent_domain::model::llm::{ChatRequest, ModelId, ProviderId, StopReason};
use agent_domain::model::message::Message;
use agent_domain::model::tool::{ToolDefinition, ToolName, ToolSafety};
use agent_domain::ports::llm::LlmProvider;
use agent_infrastructure::llm::OpenAiCompatibleProvider;
use agent_test_support::{MockLlmServer, Response};
use serde_json::{Value, json};

fn provider(base_url: &str) -> OpenAiCompatibleProvider {
    OpenAiCompatibleProvider::new(
        ProviderId::new("local"),
        base_url,
        Some("test-key".to_string()),
        ModelId::new("qwen3:8b"),
        "max_tokens",
        Duration::from_secs(5),
    )
    .unwrap()
}

fn read_file_tool() -> ToolDefinition {
    ToolDefinition {
        name: ToolName::new("read_file").unwrap(),
        description: "Read a file.".into(),
        input_schema: json!({
            "type": "object",
            "properties": {"path": {"type": "string"}},
            "required": ["path"]
        }),
        safety: ToolSafety::ReadOnly,
    }
}

/// Sends `hi` and returns whatever error the provider produced.
async fn error_from(response: Response) -> LlmError {
    let server = MockLlmServer::start(vec![response]).await;
    provider(server.base_url())
        .chat(ChatRequest::new(vec![Message::user("hi")]))
        .await
        .expect_err("the mock replied with a failure")
}

#[tokio::test]
async fn sends_a_well_formed_request_and_decodes_a_tool_call() {
    // Content *and* a tool call in one turn, with `finish_reason` disagreeing -
    // the shape Ollama actually produces.
    const RESPONSE: &str = r#"{
        "model": "qwen3:8b",
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "content": "Looking at it now.",
                "tool_calls": [{
                    "id": "call_abc",
                    "type": "function",
                    "function": {"name": "read_file", "arguments": "{\"path\":\"src/main.rs\"}"}
                }]
            },
            "finish_reason": "tool_calls"
        }],
        "usage": {"prompt_tokens": 120, "completion_tokens": 24}
    }"#;

    let server = MockLlmServer::start(vec![Response::ok(RESPONSE)]).await;

    let request = ChatRequest::new(vec![Message::user("what does src/main.rs do?")])
        .with_system("You are a coding agent.")
        .with_tools(vec![read_file_tool()]);

    let response = provider(server.base_url()).chat(request).await.unwrap();

    // --- what came back ------------------------------------------------------
    assert_eq!(response.stop_reason, StopReason::ToolUse);
    assert_eq!(response.message.text(), "Looking at it now.");
    assert_eq!(response.usage.input_tokens, 120);
    assert_eq!(response.usage.output_tokens, 24);
    assert_eq!(response.provider.as_str(), "local");

    let call = response.message.tool_calls().next().expect("a tool call");
    assert_eq!(call.name.as_str(), "read_file");
    assert_eq!(call.id.as_str(), "call_abc");
    assert_eq!(call.arguments, json!({"path": "src/main.rs"}));

    // --- what went out -------------------------------------------------------
    let sent = server.json_requests().await.remove(0);

    assert_eq!(sent["model"], json!("qwen3:8b"));
    assert_eq!(sent["max_tokens"], Value::Null, "no limit was requested");
    assert_eq!(sent["stream"], json!(false));
    assert_eq!(sent["messages"][0]["role"], json!("system"));
    assert_eq!(
        sent["messages"][0]["content"],
        json!("You are a coding agent.")
    );
    assert_eq!(sent["messages"][1]["role"], json!("user"));
    assert_eq!(sent["tools"][0]["type"], json!("function"));
    assert_eq!(sent["tools"][0]["function"]["name"], json!("read_file"));
    assert_eq!(
        sent["tools"][0]["function"]["parameters"]["required"][0],
        json!("path"),
        "the JSON schema must survive verbatim"
    );
}

#[tokio::test]
async fn a_429_becomes_a_retryable_rate_limit_error() {
    let error = error_from(Response::status(
        "429 Too Many Requests",
        r#"{"error":{"message":"slow"}}"#,
    ))
    .await;

    assert!(
        matches!(error, LlmError::RateLimited { .. }),
        "got {error:?}"
    );
    assert!(error.is_retryable());
}

#[tokio::test]
async fn a_401_becomes_a_permanent_auth_error() {
    let error = error_from(Response::status(
        "401 Unauthorized",
        r#"{"error":{"message":"invalid api key"}}"#,
    ))
    .await;

    match &error {
        LlmError::Auth(message) => assert_eq!(message, "invalid api key"),
        other => panic!("expected an auth error, got {other:?}"),
    }
    assert!(!error.is_retryable(), "retrying a bad key is pointless");
}

#[tokio::test]
async fn a_500_is_retryable_but_a_400_is_not() {
    let server_error = error_from(Response::status("500 Internal Server Error", "{}")).await;
    assert!(server_error.is_retryable(), "got {server_error:?}");

    let client_error = error_from(Response::status("400 Bad Request", "{}")).await;
    assert!(!client_error.is_retryable(), "got {client_error:?}");
}

#[tokio::test]
async fn a_non_json_body_is_reported_as_an_invalid_response() {
    // What a misconfigured reverse proxy returns.
    let error = error_from(Response::ok("<html>gateway</html>")).await;

    assert!(
        matches!(error, LlmError::InvalidResponse(_)),
        "got {error:?}"
    );
    assert!(
        error.to_string().contains("gateway"),
        "the body helps diagnose: {error}"
    );
}
