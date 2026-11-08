//! `/model`, `/compact` and `/context` through the real binary.
//!
//! The commands are parsed in `commands/chat.rs` and read the composed
//! settings, neither of which a hand-wired loop runs. So these start
//! `agent chat` against a scripted loopback model speaking the Anthropic
//! Messages API - the one adapter that hands back thinking blocks, which is
//! what a model switch has to leave behind.

use std::path::Path;
use std::process::{Output, Stdio};
use std::time::Duration;

use agent_test_support::{MockLlmServer, Response};
use serde_json::{Value, json};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

/// Only here so a hang fails the test instead of the suite.
const DEADLINE: Duration = Duration::from_secs(60);

/// `agent chat` in `workspace` against `llm`, typing `input` and closing
/// stdin (the Ctrl-D that ends the session).
async fn chat(workspace: &Path, llm: &MockLlmServer, input: &str) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_agent"))
        .env_clear()
        .env("AGENT_WORKSPACE", workspace)
        .env("AGENT_PROVIDER", "anthropic")
        .env("AGENT_API_KEY", "sk-ant-test")
        .env("AGENT_BASE_URL", llm.base_url())
        .env("AGENT_MODEL", "mock")
        .env("AGENT_STREAM", "false")
        .env("AGENT_MAX_RETRIES", "0")
        .env("AGENT_APPROVAL", "auto")
        .current_dir(workspace)
        .arg("--no-color")
        .arg("chat")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("start the agent binary");
    let mut pipe = child.stdin.take().expect("piped stdin");
    pipe.write_all(input.as_bytes()).await.expect("type");
    drop(pipe);
    let output = tokio::time::timeout(DEADLINE, child.wait_with_output())
        .await
        .unwrap_or_else(|_| panic!("the agent was still running after {DEADLINE:?}"))
        .expect("wait for the agent");
    assert_eq!(
        output.status.code(),
        Some(0),
        "exit code\n--- stdout\n{}\n--- stderr\n{}",
        text(&output.stdout),
        text(&output.stderr)
    );
    output
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// An Anthropic-shaped final answer, optionally opening with a signed
/// thinking block.
fn answer(content: Value) -> Response {
    Response::ok(
        json!({
            "id": "msg_1",
            "type": "message",
            "role": "assistant",
            "model": "mock",
            "content": content,
            "stop_reason": "end_turn",
            "usage": {
                "input_tokens": 100,
                "output_tokens": 20,
                "cache_read_input_tokens": 40
            }
        })
        .to_string(),
    )
}

fn block_types(request: &Value) -> Vec<String> {
    request["messages"]
        .as_array()
        .expect("messages")
        .iter()
        .flat_map(|message| match &message["content"] {
            Value::Array(blocks) => blocks
                .iter()
                .map(|block| block["type"].as_str().unwrap_or("?").to_string())
                .collect(),
            _ => vec!["text".to_string()],
        })
        .collect()
}

#[tokio::test]
async fn a_model_switch_reaches_the_next_request_without_the_old_models_thinking() {
    let llm = MockLlmServer::start(vec![
        answer(json!([
            {"type": "thinking", "thinking": "pondering", "signature": "SIG-FIRST"},
            {"type": "text", "text": "First."}
        ])),
        answer(json!([{"type": "text", "text": "Second."}])),
    ])
    .await;
    let workspace = tempfile::tempdir().unwrap();

    let output = chat(
        workspace.path(),
        &llm,
        "/context\nhello\n/model\n/compact\n/model other-model\n/context\nagain\n",
    )
    .await;

    let stderr = text(&output.stderr);
    let requests = llm.json_requests().await;
    assert_eq!(requests.len(), 2, "/compact had nothing to fold: {stderr}");
    assert_eq!(requests[0]["model"], "mock");
    assert_eq!(requests[1]["model"], "other-model", "{stderr}");
    let second = block_types(&requests[1]);
    assert!(
        !second.contains(&"thinking".to_string()),
        "the previous model's thinking is not sent to the new one: {second:?}"
    );
    assert!(
        requests[1].to_string().contains("First."),
        "the conversation itself carries over"
    );
    assert!(
        !requests[1].to_string().contains("SIG-FIRST"),
        "{}",
        requests[1]
    );

    assert!(stderr.contains("not built yet"), "{stderr}");
    assert!(stderr.contains("model: mock (configured)"), "{stderr}");
    assert!(stderr.contains("nothing to compact"), "{stderr}");
    assert!(stderr.contains("model: other-model (was mock)"), "{stderr}");
    assert!(stderr.contains("prompt cache starts over"), "{stderr}");
    assert!(stderr.contains("1 thinking block(s)"), "{stderr}");
    assert!(
        stderr.contains("model        other-model"),
        "/context shows the model in effect: {stderr}"
    );
    assert!(stderr.contains("cache: 40 read"), "{stderr}");
    assert_eq!(
        text(&output.stdout),
        "First.\nSecond.\n",
        "stdout carries answers only"
    );
}

#[tokio::test]
async fn a_second_context_after_a_turn_reports_the_built_system_prompt() {
    let llm = MockLlmServer::start(vec![answer(json!([{"type": "text", "text": "Hi."}]))]).await;
    let workspace = tempfile::tempdir().unwrap();

    let output = chat(workspace.path(), &llm, "hello\n/context\n").await;

    let stderr = text(&output.stderr);
    let requests = llm.json_requests().await;
    let system = &requests[0]["system"];
    let system_len: usize = match system {
        Value::String(text) => text.len(),
        Value::Array(blocks) => blocks
            .iter()
            .filter_map(|block| block["text"].as_str())
            .map(str::len)
            .sum(),
        other => panic!("unexpected system field: {other}"),
    };
    assert!(
        stderr.contains(&format!("system       {system_len} bytes")),
        "the bytes the system prompt took on the wire: {stderr}"
    );
    assert!(stderr.contains("in 2 message(s)"), "{stderr}");
}
