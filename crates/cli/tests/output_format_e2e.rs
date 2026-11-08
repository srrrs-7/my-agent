//! `agent run --output-format` through the real binary.
//!
//! The DTO mapping is unit-tested in `output/dto.rs`; what only the binary
//! shows is that stdout really carries nothing but JSON - no prose from the
//! terminal renderer, no streamed fragments, no line from some other corner
//! of the process - and that every way a run ends still leaves a result to
//! parse. These start `agent` against a scripted loopback model, with a
//! cleared environment and a fresh workspace (no `std::env::set_var`).

use std::path::Path;
use std::process::{Output, Stdio};
use std::time::{Duration, Instant};

use agent_test_support::{MockLlmServer, RecordingServer, Response};
use serde_json::Value;
use tokio::process::Command;

/// Only here so a hang fails the test instead of the suite.
const DEADLINE: Duration = Duration::from_secs(60);

fn command(workspace: &Path, base_url: &str, stream: bool, args: &[&str]) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_agent"));
    command
        .env_clear()
        .env("AGENT_WORKSPACE", workspace)
        .env("AGENT_PROVIDER", "openai")
        .env("AGENT_BASE_URL", base_url)
        .env("AGENT_MODEL", "mock")
        .env("AGENT_STREAM", if stream { "true" } else { "false" })
        .env("AGENT_MAX_RETRIES", "0")
        .current_dir(workspace)
        .arg("--no-color")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    command
}

async fn agent(llm: &MockLlmServer, stream: bool, args: &[&str]) -> Output {
    let workspace = tempfile::tempdir().unwrap();
    let child = command(workspace.path(), llm.base_url(), stream, args)
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

/// Every stdout line, parsed. Fails on the first line that is not JSON.
fn json_lines(output: &Output) -> Vec<Value> {
    let stdout = text(&output.stdout);
    assert!(
        stdout.ends_with('\n'),
        "the last line is terminated: {stdout:?}"
    );
    stdout
        .lines()
        .map(|line| {
            serde_json::from_str(line).unwrap_or_else(|error| {
                panic!(
                    "stdout line is not JSON ({error}): {line:?}\n--- stderr\n{}",
                    text(&output.stderr)
                )
            })
        })
        .collect()
}

fn describe(output: &Output) -> String {
    format!(
        "exit {:?}\n--- stdout\n{}\n--- stderr\n{}",
        output.status.code(),
        text(&output.stdout),
        text(&output.stderr)
    )
}

#[tokio::test]
async fn json_prints_exactly_one_result_object() {
    let llm = MockLlmServer::start(vec![
        Response::tool_call("call_1", "list_directory", r#"{"path":"."}"#),
        Response::assistant_text("All done."),
    ])
    .await;

    let output = agent(
        &llm,
        false,
        &["run", "--output-format", "json", "look around"],
    )
    .await;

    assert_eq!(output.status.code(), Some(0), "{}", describe(&output));
    let lines = json_lines(&output);
    assert_eq!(lines.len(), 1, "{}", describe(&output));
    let result = &lines[0];
    assert_eq!(result["v"], 1);
    assert_eq!(result["type"], "result");
    assert_eq!(result["reason"], "completed");
    assert_eq!(result["final_text"], "All done.");
    assert_eq!(result["iterations"], 2);
    assert_eq!(result["exit_code"], 0);
    assert!(
        result["session_id"]
            .as_str()
            .is_some_and(|id| !id.is_empty()),
        "{result}"
    );
    for field in [
        "input_tokens",
        "output_tokens",
        "cache_read_tokens",
        "cache_write_tokens",
    ] {
        assert!(result["usage"][field].is_u64(), "usage.{field}: {result}");
    }
    // Progress is for a person, so it stays where a person looks.
    assert!(
        text(&output.stderr).contains("list_directory"),
        "{}",
        describe(&output)
    );
}

#[tokio::test]
async fn stream_json_is_json_lines_ending_in_the_result() {
    let llm = MockLlmServer::start(vec![Response::sse_text_stream(&["Hel", "lo"])]).await;

    let output = agent(
        &llm,
        true,
        &["run", "--output-format", "stream-json", "say hello"],
    )
    .await;

    assert_eq!(output.status.code(), Some(0), "{}", describe(&output));
    let lines = json_lines(&output);
    assert!(
        lines.iter().all(|line| line["v"] == 1),
        "{}",
        describe(&output)
    );
    let types: Vec<&str> = lines
        .iter()
        .map(|line| line["type"].as_str().expect("every line has a type"))
        .collect();
    assert_eq!(types.first(), Some(&"run_started"), "{types:?}");
    assert_eq!(types.last(), Some(&"result"), "{types:?}");
    assert_eq!(
        types.iter().filter(|&&kind| kind == "result").count(),
        1,
        "{types:?}"
    );
    let streamed: String = lines
        .iter()
        .filter(|line| line["type"] == "assistant_delta")
        .map(|line| line["text"].as_str().unwrap())
        .collect();
    assert_eq!(streamed, "Hello", "the fragments arrive as events");
    assert!(types.contains(&"run_finished"), "{types:?}");
    assert_eq!(lines.last().unwrap()["final_text"], "Hello");
}

#[tokio::test]
async fn a_provider_error_in_json_mode_is_still_a_json_object() {
    let llm = MockLlmServer::start(vec![Response::status(
        "400 Bad Request",
        r#"{"error":{"message":"model is not loaded"}}"#,
    )])
    .await;

    let output = agent(&llm, false, &["run", "--output-format", "json", "hi"]).await;

    assert_eq!(output.status.code(), Some(1), "{}", describe(&output));
    let lines = json_lines(&output);
    assert_eq!(lines.len(), 1, "{}", describe(&output));
    let result = &lines[0];
    assert_eq!(result["type"], "result");
    assert_eq!(result["reason"], "error");
    assert_eq!(result["exit_code"], 1);
    assert!(
        result["error"]
            .as_str()
            .is_some_and(|error| error.contains("model is not loaded")),
        "{result}"
    );
    // The error is still reported where `text` reports it.
    assert!(
        text(&output.stderr).contains("error:"),
        "{}",
        describe(&output)
    );
}

#[tokio::test]
async fn text_stays_the_default_and_prints_the_answer_as_prose() {
    let llm = MockLlmServer::start(vec![
        Response::assistant_text("Plain answer."),
        Response::assistant_text("Plain answer."),
    ])
    .await;

    let default = agent(&llm, false, &["run", "hi"]).await;
    let explicit = agent(&llm, false, &["run", "--output-format", "text", "hi"]).await;

    for output in [default, explicit] {
        assert_eq!(output.status.code(), Some(0), "{}", describe(&output));
        assert_eq!(
            text(&output.stdout),
            "Plain answer.\n",
            "{}",
            describe(&output)
        );
    }
}

#[tokio::test]
async fn an_interrupted_json_run_reports_it_and_exits_130() {
    let model = RecordingServer::unresponsive().await;
    let workspace = tempfile::tempdir().unwrap();
    let base_url = format!("{}/v1", model.url());
    let child = command(
        workspace.path(),
        &base_url,
        false,
        &["run", "--output-format", "json", "refactor everything"],
    )
    .spawn()
    .expect("start the agent binary");

    // Mid-request is observed, not timed: wait until the request arrived.
    let deadline = Instant::now() + DEADLINE;
    while model.received().is_empty() {
        assert!(Instant::now() < deadline, "the model was never asked");
        tokio::task::yield_now().await;
    }
    let pid = child.id().expect("still running");
    let status = std::process::Command::new("/bin/sh")
        .arg("-c")
        .arg(format!("kill -INT {pid}"))
        .status()
        .expect("run kill");
    assert!(status.success(), "kill -INT {pid} failed");

    let output = tokio::time::timeout(DEADLINE, child.wait_with_output())
        .await
        .expect("the agent stops after Ctrl-C")
        .expect("wait for the agent");

    assert_eq!(output.status.code(), Some(130), "{}", describe(&output));
    let lines = json_lines(&output);
    assert_eq!(lines.len(), 1, "{}", describe(&output));
    assert_eq!(lines[0]["reason"], "interrupted");
    assert_eq!(lines[0]["exit_code"], 130);
}
