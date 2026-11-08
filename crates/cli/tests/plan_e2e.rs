//! The work plan through the real binary.
//!
//! `todo_write` is registered in the composition root, the resumed plan is
//! restored in `commands/chat.rs`, and the plan is drawn by the terminal
//! renderer - three places `end_to_end.rs`, which wires the loop by hand, never
//! runs. So these start `agent` itself against a scripted loopback model, with
//! a cleared environment and a fresh workspace (no `std::env::set_var`).

use std::path::Path;
use std::process::{Output, Stdio};
use std::time::Duration;

use agent_test_support::{MockLlmServer, Response};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

/// Only here so a hang fails the test instead of the suite.
const DEADLINE: Duration = Duration::from_secs(60);

const PLAN: &str = r#"{"todos":[{"content":"read the grammar","status":"in_progress"},{"content":"write the lexer","status":"pending"}]}"#;

/// Runs `agent <args>` in `workspace` against `llm`, typing `stdin` if given.
///
/// The approval policy is left at its default (`read-only`) and stdin is never
/// a terminal, so any call that needed confirmation would be denied - which is
/// what makes "the plan was written" also mean "nobody was asked".
async fn agent(
    workspace: &Path,
    llm: &MockLlmServer,
    args: &[&str],
    stdin: Option<&str>,
) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_agent"));
    command
        .env_clear()
        .env("AGENT_WORKSPACE", workspace)
        .env("AGENT_PROVIDER", "openai")
        .env("AGENT_BASE_URL", llm.base_url())
        .env("AGENT_MODEL", "mock")
        .env("AGENT_STREAM", "false")
        .env("AGENT_MAX_RETRIES", "0")
        .current_dir(workspace)
        .arg("--no-color")
        .args(args)
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let mut child = command.spawn().expect("start the agent binary");
    if let Some(input) = stdin {
        let mut pipe = child.stdin.take().expect("piped stdin");
        pipe.write_all(input.as_bytes()).await.expect("type");
        // Dropping the pipe is the Ctrl-D that ends the session.
    }
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

#[tokio::test]
async fn the_plan_is_written_without_approval_and_drawn_on_stderr_only() {
    let llm = MockLlmServer::start(vec![
        Response::tool_call("call_1", "todo_write", PLAN),
        Response::assistant_text("Planned."),
    ])
    .await;
    let workspace = tempfile::tempdir().unwrap();

    let output = agent(workspace.path(), &llm, &["run", "plan it"], None).await;

    let stderr = text(&output.stderr);
    assert!(stderr.contains("plan (0/2 completed)"), "{stderr}");
    assert!(stderr.contains("[>] read the grammar"), "{stderr}");
    assert!(stderr.contains("[ ] write the lexer"), "{stderr}");
    assert_eq!(
        text(&output.stdout),
        "Planned.\n",
        "stdout carries the answer only"
    );

    let requests = llm.json_requests().await;
    let advertised: Vec<&str> = requests[0]["tools"]
        .as_array()
        .expect("tools are advertised")
        .iter()
        .filter_map(|tool| tool["function"]["name"].as_str())
        .collect();
    assert!(advertised.contains(&"todo_write"), "{advertised:?}");
    let result = requests[1]["messages"]
        .as_array()
        .expect("messages")
        .iter()
        .find(|message| message["role"] == "tool")
        .expect("the call was answered")["content"]
        .to_string();
    assert!(
        result.contains("Plan updated"),
        "not approved or not run: {result}"
    );
}

#[tokio::test]
async fn a_resumed_chat_gets_its_plan_back_and_reset_drops_it() {
    let llm = MockLlmServer::start(vec![
        Response::tool_call("call_1", "todo_write", PLAN),
        Response::assistant_text("Planned."),
    ])
    .await;
    let workspace = tempfile::tempdir().unwrap();

    agent(workspace.path(), &llm, &["chat"], Some("plan it\n")).await;
    let resumed = agent(
        workspace.path(),
        &llm,
        &["chat", "--resume"],
        Some("/plan\n/reset\n/plan\n/exit\n"),
    )
    .await;

    let stderr = text(&resumed.stderr);
    assert!(stderr.contains("resuming"), "{stderr}");
    assert!(
        stderr.contains("with a plan of 2 step(s), 0 completed"),
        "{stderr}"
    );
    let (before, after) = stderr
        .split_once("conversation cleared")
        .expect("/reset ran");
    assert!(
        before.contains("[>] read the grammar"),
        "/plan showed the restored plan: {before}"
    );
    assert!(
        after.contains("no plan"),
        "/reset dropped the plan: {after}"
    );
}
