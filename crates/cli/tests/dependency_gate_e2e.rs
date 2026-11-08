//! The dependency gate through the real binary.
//!
//! The gate is assembled in three places no other test runs together: the
//! dispatcher asks, `CliApprovalGate` answers (from `AGENT_APPROVAL` and
//! `AGENT_ALLOW_DEPENDENCY_CHANGES`), and the composition root wires them. So
//! these start `agent` itself against a scripted loopback model, in a fresh
//! workspace, with a cleared environment and stdin that is not a terminal.

use std::path::Path;
use std::process::{Output, Stdio};
use std::time::Duration;

use agent_test_support::{MockLlmServer, Response};
use tokio::process::Command;

/// Only here so a hang fails the test instead of the suite.
const DEADLINE: Duration = Duration::from_secs(60);

const MANIFEST: &str = "[package]\nname = \"demo\"\n\n[dependencies]\nserde = \"1\"\n";

const ADD_TOKIO: &str = r#"{"path":"Cargo.toml","old_string":"serde = \"1\"\n","new_string":"serde = \"1\"\ntokio = \"1\"\n"}"#;

async fn agent(workspace: &Path, llm: &MockLlmServer, env: &[(&str, &str)]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_agent"));
    command
        .env_clear()
        .env("AGENT_WORKSPACE", workspace)
        .env("AGENT_PROVIDER", "openai")
        .env("AGENT_BASE_URL", llm.base_url())
        .env("AGENT_MODEL", "mock")
        .env("AGENT_STREAM", "false")
        .env("AGENT_MAX_RETRIES", "0")
        .env("AGENT_SESSION_LOG", "false")
        .envs(env.iter().copied())
        .current_dir(workspace)
        .args(["--no-color", "run", "add tokio"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let output = tokio::time::timeout(DEADLINE, command.output())
        .await
        .unwrap_or_else(|_| panic!("the agent was still running after {DEADLINE:?}"))
        .expect("run the agent binary");
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

/// The content of the tool result the model received in its second request.
async fn tool_result(llm: MockLlmServer) -> String {
    let requests = llm.json_requests().await;
    requests[1]["messages"]
        .as_array()
        .expect("messages")
        .iter()
        .find(|message| message["role"] == "tool")
        .expect("the call was answered")["content"]
        .to_string()
}

#[tokio::test]
async fn an_unattended_auto_run_cannot_change_the_manifest() {
    let llm = MockLlmServer::start(vec![
        Response::tool_call("call_1", "edit_file", ADD_TOKIO),
        Response::assistant_text("I could not add tokio."),
    ])
    .await;
    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(workspace.path().join("Cargo.toml"), MANIFEST).unwrap();

    let output = agent(workspace.path(), &llm, &[("AGENT_APPROVAL", "auto")]).await;

    assert_eq!(
        std::fs::read_to_string(workspace.path().join("Cargo.toml")).unwrap(),
        MANIFEST,
        "the manifest must be untouched"
    );
    let result = tool_result(llm).await;
    assert!(result.contains("Not executed"), "{result}");
    assert!(!result.contains("--yes"), "{result}");
    let stderr = text(&output.stderr);
    assert!(stderr.contains("edit_file denied"), "{stderr}");
}

#[tokio::test]
async fn the_operator_can_allow_dependency_changes_for_an_unattended_run() {
    let llm = MockLlmServer::start(vec![
        Response::tool_call("call_1", "edit_file", ADD_TOKIO),
        Response::assistant_text("Added tokio."),
    ])
    .await;
    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(workspace.path().join("Cargo.toml"), MANIFEST).unwrap();

    agent(
        workspace.path(),
        &llm,
        &[
            ("AGENT_APPROVAL", "auto"),
            ("AGENT_ALLOW_DEPENDENCY_CHANGES", "true"),
        ],
    )
    .await;

    let manifest = std::fs::read_to_string(workspace.path().join("Cargo.toml")).unwrap();
    assert!(manifest.contains("tokio = \"1\""), "{manifest}");
}
