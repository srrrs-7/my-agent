//! `spawn_agent` through the real binary.
//!
//! The parent's model delegates, the sub-agent reads a file and sums it up,
//! and the parent answers from the summary. What only the binary shows: the
//! sub-agent's answer stays off stdout, its activity is drawn under the call
//! that started it, its record is kept on disk - and `agent sessions` still
//! lists the one session the user had. A scripted loopback model, a cleared
//! environment and a fresh workspace (no `std::env::set_var`).

use std::path::Path;
use std::process::{Output, Stdio};
use std::time::Duration;

use agent_test_support::{MockLlmServer, Response};
use serde_json::Value;
use tokio::process::Command;

/// Only here so a hang fails the test instead of the suite.
const DEADLINE: Duration = Duration::from_secs(60);

const NOTES: &str = "launch codes: 0000";
const SUMMARY: &str = "notes.txt holds the launch codes.";
const ANSWER: &str = "Your notes hold the launch codes.";

async fn agent(workspace: &Path, base_url: &str, args: &[&str]) -> Output {
    let child = Command::new(env!("CARGO_BIN_EXE_agent"))
        .env_clear()
        .env("AGENT_WORKSPACE", workspace)
        .env("AGENT_PROVIDER", "openai")
        .env("AGENT_BASE_URL", base_url)
        .env("AGENT_MODEL", "mock")
        .env("AGENT_STREAM", "false")
        .env("AGENT_MAX_RETRIES", "0")
        .current_dir(workspace)
        .arg("--no-color")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("start the agent binary");
    tokio::time::timeout(DEADLINE, child.wait_with_output())
        .await
        .unwrap_or_else(|_| panic!("the agent was still running after {DEADLINE:?}"))
        .expect("wait for the agent")
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn describe(output: &Output) -> String {
    format!(
        "exit {:?}\n--- stdout\n{}\n--- stderr\n{}",
        output.status.code(),
        text(&output.stdout),
        text(&output.stderr)
    )
}

/// Every `.jsonl` file directly in `directory`, by stem.
fn sessions_in(directory: &Path) -> Vec<String> {
    let mut stems: Vec<String> = std::fs::read_dir(directory)
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "jsonl")
        })
        .map(|path| path.file_stem().unwrap().to_string_lossy().into_owned())
        .collect();
    stems.sort();
    stems
}

/// The text of every message in an OpenAI-shaped request.
fn said(request: &Value) -> String {
    request["messages"].to_string()
}

#[tokio::test]
async fn the_parent_answers_from_the_sub_agents_summary_and_only_its_own_session_is_listed() {
    let llm = MockLlmServer::start(vec![
        Response::tool_call(
            "call_spawn",
            "spawn_agent",
            r#"{"task":"Read notes.txt and say what it holds."}"#,
        ),
        Response::tool_call("call_read", "read_file", r#"{"path":"notes.txt"}"#),
        Response::assistant_text(SUMMARY),
        Response::assistant_text(ANSWER),
    ])
    .await;
    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(workspace.path().join("notes.txt"), NOTES).unwrap();

    let output = agent(
        workspace.path(),
        llm.base_url(),
        &["run", "What is in my notes?"],
    )
    .await;

    assert_eq!(output.status.code(), Some(0), "{}", describe(&output));
    assert_eq!(
        text(&output.stdout),
        format!("{ANSWER}\n"),
        "stdout is the user's answer alone, not the sub-agent's: {}",
        describe(&output)
    );
    let stderr = text(&output.stderr);
    assert!(
        stderr.contains("→ spawn_agent") && stderr.contains("\n  → read_file"),
        "the sub-agent's call is drawn under the one that started it: {stderr}"
    );

    // The parent's model saw the summary, and nothing of what was read.
    let requests = llm.json_requests().await;
    assert_eq!(requests.len(), 4, "{requests:?}");
    let last = said(&requests[3]);
    assert!(last.contains(SUMMARY), "{last}");
    assert!(!last.contains(NOTES), "the file reached the parent: {last}");
    assert!(said(&requests[2]).contains(NOTES), "the sub-agent read it");

    // One session, and the sub-agent's record beside it rather than among
    // the sessions.
    let directory = workspace.path().join(".agent/sessions");
    let sessions = sessions_in(&directory);
    assert_eq!(sessions.len(), 1, "{sessions:?}");
    let parent = &sessions[0];
    let child = directory.join(format!("subagents/{parent}/call_spawn.jsonl"));
    let record = std::fs::read_to_string(&child)
        .unwrap_or_else(|error| panic!("no sub-agent record at {child:?}: {error}"));
    assert!(record.contains(NOTES), "{record}");

    let listing = agent(workspace.path(), "http://127.0.0.1:9/v1", &["sessions"]).await;
    assert_eq!(listing.status.code(), Some(0), "{}", describe(&listing));
    let listed = text(&listing.stdout);
    let rows: Vec<&str> = listed
        .lines()
        .filter(|line| line.contains(parent.as_str()))
        .collect();
    assert_eq!(rows.len(), 1, "{listed}");
    assert!(
        !listed.contains("call_spawn") && !listed.contains("subagents"),
        "the sub-agent is listed as a session: {listed}"
    );
}
