//! Hooks through the real binary: the stop hook is awaited by the subcommand
//! when a turn ends, which only `agent run` itself shows.

use std::path::Path;
use std::process::{Output, Stdio};
use std::time::Duration;

use agent_test_support::{RecordingServer, Response};
use serde_json::{Value, json};
use tokio::process::Command;

/// Only here so a hang fails the test instead of the suite.
const DEADLINE: Duration = Duration::from_secs(60);

async fn run(workspace: &Path, hooks: &Path, model: &RecordingServer) -> Output {
    let base_url = format!("{}/v1", model.url());
    let mut command = Command::new(env!("CARGO_BIN_EXE_agent"));
    command
        .env_clear()
        // The hook's shell finds `cat` on it; nothing else here needs it.
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("AGENT_WORKSPACE", workspace)
        .env("AGENT_PROVIDER", "openai")
        .env("AGENT_BASE_URL", &base_url)
        .env("AGENT_MODEL", "mock")
        .env("AGENT_STREAM", "false")
        .env("AGENT_MAX_RETRIES", "0")
        .env("AGENT_SESSION_LOG", "false")
        .env("AGENT_HOOKS", hooks)
        // Where the hook runs is not the point here; `tests/hooks.rs` in the
        // infrastructure crate proves the confinement.
        .env("AGENT_HOOKS_SANDBOX", "none")
        .current_dir(workspace)
        .args(["--no-color", "run", "hi"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    tokio::time::timeout(DEADLINE, command.output())
        .await
        .unwrap_or_else(|_| panic!("agent run was still running after {DEADLINE:?}"))
        .expect("run the agent binary")
}

#[tokio::test]
async fn the_stop_hook_runs_when_the_turn_ends_and_is_told_how() {
    let workspace = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let hooks = outside.path().join("hooks.json");
    std::fs::write(
        &hooks,
        json!({"stop": [{"command": "cat > stop.json"}]}).to_string(),
    )
    .unwrap();
    let model = RecordingServer::start(Response::assistant_text("done")).await;

    let output = run(workspace.path(), &hooks, &model).await;
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "--- stderr\n{stderr}");

    // Awaited before `run` returns, so the file is there once the process
    // has exited - no polling.
    let text = std::fs::read_to_string(workspace.path().join("stop.json"))
        .unwrap_or_else(|error| panic!("the stop hook did not run: {error}\n{stderr}"));
    let seen: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(seen["v"], 1);
    assert_eq!(seen["event"], "stop");
    assert_eq!(seen["reason"], "completed");
    assert!(
        seen["session_id"].as_str().is_some_and(|id| !id.is_empty()),
        "{seen}"
    );
}
