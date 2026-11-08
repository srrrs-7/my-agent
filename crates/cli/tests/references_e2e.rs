//! `@path` references through the real binary.
//!
//! The lexer and the file system meet in `commands/chat.rs` and the
//! composition root, neither of which a hand-wired test runs. So this starts
//! `agent chat` against a scripted loopback model and reads what reached it.

use std::path::Path;
use std::process::{Output, Stdio};
use std::time::Duration;

use agent_test_support::{MockLlmServer, Response};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

/// Only here so a hang fails the test instead of the suite.
const DEADLINE: Duration = Duration::from_secs(60);

async fn chat(workspace: &Path, llm: &MockLlmServer, input: &str) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_agent"))
        .env_clear()
        .env("AGENT_WORKSPACE", workspace)
        .env("AGENT_PROVIDER", "openai")
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
        "exit code\n--- stderr\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

/// The content of the last user message of the first request.
fn first_user_message(requests: &[serde_json::Value]) -> String {
    requests.first().expect("the model was asked")["messages"]
        .as_array()
        .expect("messages")
        .iter()
        .rev()
        .find(|message| message["role"] == "user")
        .and_then(|message| message["content"].as_str())
        .expect("a user message with text content")
        .to_string()
}

#[tokio::test]
async fn a_referenced_file_reaches_the_model_inside_the_delimiter() {
    let llm = MockLlmServer::start(vec![Response::assistant_text("Seen.")]).await;
    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(
        workspace.path().join("notes.txt"),
        "the launch is on friday\n",
    )
    .unwrap();

    let output = chat(workspace.path(), &llm, "look at @notes.txt\n").await;

    let requests = llm.json_requests().await;
    let message = first_user_message(&requests);
    assert!(
        message.starts_with("look at @notes.txt"),
        "what was typed is kept: {message}"
    );
    assert!(
        message.contains("<file path=\"notes.txt\">\nthe launch is on friday\n</file>"),
        "{message}"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("attached @notes.txt"), "{stderr}");
}

#[tokio::test]
async fn a_protected_file_is_not_attached_and_stderr_says_why() {
    let llm = MockLlmServer::start(vec![Response::assistant_text("Fine.")]).await;
    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(workspace.path().join(".env"), "API_KEY=do-not-send\n").unwrap();

    let output = chat(workspace.path(), &llm, "what is in @.env\n").await;

    let requests = llm.json_requests().await;
    let message = first_user_message(&requests);
    assert_eq!(
        message, "what is in @.env",
        "the turn ran with the line as typed"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("not attached @.env: protected by the workspace access policy"),
        "{stderr}"
    );
    assert!(
        !serde_json::to_string(&requests)
            .unwrap()
            .contains("do-not-send"),
        "the secret never left"
    );
}
