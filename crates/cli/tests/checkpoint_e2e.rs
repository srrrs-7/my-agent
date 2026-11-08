//! `/undo` and `/rewind` through the real binary.
//!
//! The journal is wired in the composition root and the commands are parsed in
//! `commands/chat.rs` - neither of which `end_to_end.rs`, wiring the loop by
//! hand, ever runs. So these start `agent chat` itself against a scripted
//! loopback model, with a cleared environment and a fresh workspace, and type
//! into it through a pipe.

use std::path::Path;
use std::process::{Output, Stdio};
use std::time::Duration;

use agent_test_support::{MockLlmServer, Response};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};

/// Only here so a hang fails the test instead of the suite.
const DEADLINE: Duration = Duration::from_secs(60);

/// `agent <args>` in `workspace` against `llm`, approving every call: stdin is
/// a pipe, so nobody could answer a prompt.
fn spawn(workspace: &Path, llm: &MockLlmServer, args: &[&str]) -> Child {
    Command::new(env!("CARGO_BIN_EXE_agent"))
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
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("start the agent binary")
}

/// Types `input`, closes stdin (the Ctrl-D that ends the session) and waits.
async fn finish(mut child: Child, input: &str) -> Output {
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

async fn chat(workspace: &Path, llm: &MockLlmServer, args: &[&str], input: &str) -> Output {
    finish(spawn(workspace, llm, args), input).await
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn write_call(id: &str, path: &str, content: &str) -> Response {
    Response::tool_call(
        id,
        "write_file",
        &serde_json::json!({"path": path, "content": content}).to_string(),
    )
}

#[tokio::test]
async fn undo_puts_back_the_files_of_the_last_turn() {
    let llm = MockLlmServer::start(vec![
        write_call("call_1", "notes.txt", "rewritten\n"),
        write_call("call_2", "created.txt", "new\n"),
        Response::assistant_text("Written."),
    ])
    .await;
    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(workspace.path().join("notes.txt"), "original\n").unwrap();

    let output = chat(workspace.path(), &llm, &["chat"], "write them\n/undo\n").await;

    let stderr = text(&output.stderr);
    assert_eq!(
        std::fs::read_to_string(workspace.path().join("notes.txt")).unwrap(),
        "original\n",
        "{stderr}"
    );
    assert!(
        !workspace.path().join("created.txt").exists(),
        "a file the turn created is deleted: {stderr}"
    );
    assert!(stderr.contains("restored  notes.txt"), "{stderr}");
    assert!(stderr.contains("deleted   created.txt"), "{stderr}");
    assert_eq!(
        text(&output.stdout),
        "Written.\n",
        "stdout carries answers only"
    );
}

#[tokio::test]
async fn undo_leaves_a_file_changed_since_as_it_is() {
    let llm = MockLlmServer::start(vec![
        write_call("call_1", "notes.txt", "by the agent\n"),
        Response::assistant_text("Written."),
    ])
    .await;
    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(workspace.path().join("notes.txt"), "original\n").unwrap();
    let mut child = spawn(workspace.path(), &llm, &["chat"]);

    // Waits for the answer on stdout, so the turn - and its write - is over
    // before the file is touched behind the agent's back.
    let mut stdin = child.stdin.take().expect("piped stdin");
    stdin.write_all(b"write it\n").await.unwrap();
    let mut stdout = BufReader::new(child.stdout.take().expect("piped stdout")).lines();
    let answer = tokio::time::timeout(DEADLINE, stdout.next_line())
        .await
        .expect("an answer in time")
        .unwrap();
    assert_eq!(answer.as_deref(), Some("Written."));
    std::fs::write(workspace.path().join("notes.txt"), "edited by hand\n").unwrap();
    child.stdin = Some(stdin);

    let output = finish(child, "/undo\n").await;

    let stderr = text(&output.stderr);
    assert_eq!(
        std::fs::read_to_string(workspace.path().join("notes.txt")).unwrap(),
        "edited by hand\n",
        "a conflicting file is never overwritten: {stderr}"
    );
    assert!(stderr.contains("conflict  notes.txt"), "{stderr}");
}

#[tokio::test]
async fn a_resumed_session_continues_from_after_the_rewind() {
    let llm = MockLlmServer::start(vec![
        Response::assistant_text("First answer."),
        write_call("call_1", "notes.txt", "rewound away\n"),
        Response::assistant_text("Second answer."),
        Response::assistant_text("Third answer."),
    ])
    .await;
    let workspace = tempfile::tempdir().unwrap();

    let first = chat(
        workspace.path(),
        &llm,
        &["chat"],
        "first question\nsecond question\n/rewind\n",
    )
    .await;
    let stderr = text(&first.stderr);
    assert!(
        !workspace.path().join("notes.txt").exists(),
        "the rewound turn's file is gone: {stderr}"
    );
    assert!(
        stderr.contains("conversation rewound by 1 turn(s)"),
        "{stderr}"
    );

    chat(
        workspace.path(),
        &llm,
        &["chat", "--resume"],
        "third question\n",
    )
    .await;

    let requests = llm.json_requests().await;
    let last: Vec<String> = requests
        .last()
        .expect("the resumed session asked the model")["messages"]
        .as_array()
        .expect("messages")
        .iter()
        .filter_map(|message| message["content"].as_str().map(str::to_owned))
        .collect();
    assert!(
        last.iter().any(|content| content == "first question"),
        "{last:?}"
    );
    assert!(
        last.iter().any(|content| content == "third question"),
        "{last:?}"
    );
    assert!(
        !last
            .iter()
            .any(|content| content == "second question" || content == "Second answer."),
        "the rewound turn stays gone after a resume: {last:?}"
    );
}
