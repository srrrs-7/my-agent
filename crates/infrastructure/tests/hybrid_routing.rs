//! Local-first hybrid execution over real sockets: two mock servers stand in
//! for a local Ollama and a cloud endpoint, and the whole stack is built from
//! environment variables exactly as `agent` builds it.
//!
//! Both servers speak the OpenAI protocol so one mock covers them; the cloud
//! side being Anthropic in practice changes nothing about where a request is
//! sent.

use agent_domain::model::llm::{ChatRequest, RequestMetadata, TaskKind};
use agent_domain::model::message::Message;
use agent_domain::ports::llm::LlmProvider;
use agent_infrastructure::config::{MapEnv, Settings};
use agent_infrastructure::llm::build_provider;
use agent_test_support::{MockLlmServer, Response};
use std::sync::Arc;

fn stack(
    local: &MockLlmServer,
    cloud: &MockLlmServer,
    extra: &[(&str, &str)],
) -> Arc<dyn LlmProvider> {
    let mut env = MapEnv::new(&[
        ("AGENT_WORKSPACE", "/workspace"),
        ("AGENT_PROVIDERS", "local,cloud"),
        ("AGENT_PROVIDER_LOCAL_KIND", "ollama"),
        ("AGENT_PROVIDER_LOCAL_BASE_URL", local.base_url()),
        ("AGENT_PROVIDER_LOCAL_MODEL", "qwen3:8b"),
        ("AGENT_PROVIDER_CLOUD_KIND", "openai"),
        ("AGENT_PROVIDER_CLOUD_BASE_URL", cloud.base_url()),
        ("AGENT_PROVIDER_CLOUD_API_KEY", "sk-cloud"),
        ("AGENT_PROVIDER_CLOUD_MODEL", "cloud-large"),
        ("AGENT_DEFAULT_PROVIDER", "cloud"),
        // One attempt per destination: the retry policy has its own tests,
        // and its backoff would only slow these down.
        ("AGENT_MAX_RETRIES", "1"),
    ]);
    for (key, value) in extra {
        env.set(key, value);
    }
    let settings = Settings::from_source(&env).expect("valid configuration");
    build_provider(&settings.llm).expect("the stack builds")
}

/// A request shaped like the one the history compactor sends.
fn summary_request() -> ChatRequest {
    ChatRequest::new(vec![Message::user("## user\nfix the bug")]).with_metadata(RequestMetadata {
        task_kind: TaskKind::Summarize,
        ..RequestMetadata::default()
    })
}

fn agentic_request() -> ChatRequest {
    ChatRequest::new(vec![Message::user("fix the bug")])
}

#[tokio::test]
async fn a_summary_goes_to_the_local_server_while_the_agent_loop_goes_to_the_cloud() {
    let local = MockLlmServer::start(vec![Response::assistant_text("the summary")]).await;
    let cloud = MockLlmServer::start(vec![Response::assistant_text("the answer")]).await;
    let provider = stack(&local, &cloud, &[("AGENT_ROUTE_SUMMARIZE", "local")]);

    let summary = provider.chat(summary_request()).await.unwrap();
    let answer = provider.chat(agentic_request()).await.unwrap();

    assert_eq!(summary.message.text(), "the summary");
    assert_eq!(summary.provider.as_str(), "local");
    assert_eq!(answer.message.text(), "the answer");
    assert_eq!(answer.provider.as_str(), "cloud");

    let local_bodies = local.json_requests().await;
    let cloud_bodies = cloud.json_requests().await;
    assert_eq!(local_bodies.len(), 1);
    assert_eq!(local_bodies[0]["model"], "qwen3:8b");
    assert_eq!(cloud_bodies.len(), 1);
    assert_eq!(cloud_bodies[0]["model"], "cloud-large");
}

#[tokio::test]
async fn a_request_the_cloud_rejects_with_503_succeeds_on_the_local_server() {
    let local = MockLlmServer::start(vec![Response::assistant_text("served locally")]).await;
    let cloud = MockLlmServer::start(vec![Response::status(
        "503 Service Unavailable",
        r#"{"error":{"message":"overloaded"}}"#,
    )])
    .await;
    let provider = stack(&local, &cloud, &[("AGENT_FAILOVER", "local")]);

    let response = provider.chat(agentic_request()).await.unwrap();

    assert_eq!(response.message.text(), "served locally");
    assert_eq!(response.provider.as_str(), "local");
    let local_bodies = local.json_requests().await;
    assert_eq!(
        local_bodies[0]["model"], "qwen3:8b",
        "the fallback is asked for its own model, not the cloud's"
    );
    assert_eq!(cloud.json_requests().await.len(), 1);
}

#[tokio::test]
async fn without_a_failover_chain_a_cloud_outage_is_still_an_error() {
    let local = MockLlmServer::start(Vec::new()).await;
    let cloud = MockLlmServer::start(vec![Response::status(
        "503 Service Unavailable",
        r#"{"error":{"message":"overloaded"}}"#,
    )])
    .await;
    let provider = stack(&local, &cloud, &[]);

    assert!(provider.chat(agentic_request()).await.is_err());
    assert!(local.requests().await.is_empty(), "nothing failed over");
}
