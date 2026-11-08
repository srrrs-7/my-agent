//! Exercises the Anthropic client over a real socket.
//!
//! The unit tests in `llm::anthropic` pin the payload mapping; this checks what
//! actually leaves the process - the prompt-cache markers in the body, the
//! absence of any beta header - and that the cache counts in a real-shaped
//! reply come back out.

use std::time::Duration;

use agent_domain::model::llm::{ChatRequest, ModelId, ProviderId, TokenUsage};
use agent_domain::model::message::Message;
use agent_domain::model::tool::{ToolDefinition, ToolName, ToolSafety};
use agent_domain::ports::llm::LlmProvider;
use agent_infrastructure::llm::{AnthropicProvider, PromptCache};
use agent_test_support::{RecordingServer, Response};
use serde_json::json;

const SYSTEM: &str = "You are a coding agent.";

/// A reply that read most of its prompt from the cache.
const REPLY: &str = r#"{
    "id": "msg_1",
    "type": "message",
    "role": "assistant",
    "model": "claude-opus-5-5",
    "content": [{"type": "text", "text": "ok"}],
    "stop_reason": "end_turn",
    "usage": {
        "input_tokens": 12,
        "output_tokens": 3,
        "cache_creation_input_tokens": 40,
        "cache_read_input_tokens": 2048
    }
}"#;

fn provider(base_url: &str, prompt_cache: PromptCache) -> AnthropicProvider {
    AnthropicProvider::new(
        ProviderId::new("cloud"),
        base_url,
        "sk-ant-test".to_string(),
        ModelId::new("claude-opus-5-5"),
        Duration::from_secs(5),
    )
    .unwrap()
    .with_prompt_cache(prompt_cache)
}

fn tool(name: &str) -> ToolDefinition {
    ToolDefinition {
        name: ToolName::new(name).unwrap(),
        description: format!("{name} a file."),
        input_schema: json!({
            "type": "object",
            "properties": {"path": {"type": "string"}},
            "required": ["path"]
        }),
        safety: ToolSafety::ReadOnly,
    }
}

/// Two consecutive turns of one conversation, as the loop would send them.
fn two_turns() -> [ChatRequest; 2] {
    let tools = vec![tool("read_file"), tool("write_file")];
    [
        ChatRequest::new(vec![Message::user("hi")])
            .with_system(SYSTEM)
            .with_tools(tools.clone()),
        ChatRequest::new(vec![
            Message::user("hi"),
            Message::assistant_text("ok"),
            Message::user("and now?"),
        ])
        .with_system(SYSTEM)
        .with_tools(tools),
    ]
}

#[tokio::test]
async fn every_turn_marks_the_same_prefix_and_asks_for_automatic_caching() {
    let server = RecordingServer::start(Response::ok(REPLY)).await;
    let client = provider(server.url(), PromptCache::default());

    let [first, second] = two_turns();
    let response = client.chat(first).await.unwrap();
    client.chat(second).await.unwrap();

    assert_eq!(
        response.usage,
        TokenUsage {
            input_tokens: 2100,
            output_tokens: 3,
            cache_read_tokens: 2048,
            cache_write_tokens: 40,
        }
    );

    let received = server.received();
    assert_eq!(received.len(), 2);
    for request in &received {
        assert_eq!(request.path, "/v1/messages");
        assert_eq!(request.header("anthropic-version"), Some("2023-06-01"));
        assert_eq!(
            request.header("anthropic-beta"),
            None,
            "neither cache feature needs a beta header"
        );

        let body = request.json();
        assert_eq!(
            body["system"],
            json!([{
                "type": "text",
                "text": SYSTEM,
                "cache_control": {"type": "ephemeral"}
            }])
        );
        assert_eq!(body["cache_control"], json!({"type": "ephemeral"}));
    }

    // What sits in front of the conversation must not move between turns, or
    // the second turn cannot read what the first one wrote.
    let (first, second) = (received[0].json(), received[1].json());
    assert_eq!(first["tools"], second["tools"]);
    assert_eq!(first["system"], second["system"]);
}

#[tokio::test]
async fn a_one_hour_ttl_reaches_the_wire_on_both_breakpoints() {
    let server = RecordingServer::start(Response::ok(REPLY)).await;
    let [request, _] = two_turns();

    provider(server.url(), PromptCache::OneHour)
        .chat(request)
        .await
        .unwrap();

    let body = server.received().remove(0).json();
    let one_hour = json!({"type": "ephemeral", "ttl": "1h"});
    assert_eq!(body["system"][0]["cache_control"], one_hour);
    assert_eq!(body["cache_control"], one_hour);
}

#[tokio::test]
async fn with_caching_off_no_marker_leaves_the_process() {
    let server = RecordingServer::start(Response::ok(REPLY)).await;
    let [request, _] = two_turns();

    provider(server.url(), PromptCache::Off)
        .chat(request)
        .await
        .unwrap();

    let sent = server.received().remove(0);
    assert!(!sent.body.contains("cache_control"), "{}", sent.body);
}
