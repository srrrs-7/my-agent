//! `agent eval` through the real binary.
//!
//! The suite is read, every attempt is assembled by the real composition
//! root over a scratch copy of its fixture, and the report is printed - all
//! of which only the binary does. So this starts `agent` itself against a
//! scripted loopback model, with a cleared environment (no
//! `std::env::set_var`) and a private temp dir, so that what the run leaves
//! behind can be inspected.

use std::path::Path;
use std::process::{Output, Stdio};
use std::time::{Duration, Instant};

use agent_test_support::{MockLlmServer, RecordingServer, Response};
use serde_json::Value;
use tokio::process::Command;

/// Only here so a hang fails the test instead of the suite.
const DEADLINE: Duration = Duration::from_secs(60);

/// What `agent eval` documents for "some tasks failed".
const EXIT_TASKS_FAILED: i32 = 3;

const SUITE: &str = r#"{
  "version": 1,
  "tasks": [
    {
      "id": "write-hello",
      "fixture": "fixtures/base",
      "prompt": "Create hello.txt saying hello eval.",
      "checks": [
        {"type": "file_contains", "path": "hello.txt", "contains": "hello eval"},
        {"type": "file_contains", "path": "README.md", "contains": "fixture"}
      ]
    },
    {
      "id": "never-writes",
      "fixture": "fixtures/base",
      "prompt": "Create bye.txt.",
      "checks": [
        {"type": "file_contains", "path": "bye.txt", "contains": "bye"}
      ]
    }
  ]
}"#;

async fn eval(root: &Path, llm: &MockLlmServer, args: &[&str]) -> Output {
    let mut command = command(root, llm.base_url(), args);
    tokio::time::timeout(DEADLINE, command.output())
        .await
        .unwrap_or_else(|_| panic!("eval was still running after {DEADLINE:?}"))
        .expect("run the agent binary")
}

/// `agent eval <args>` in `root`, against the model at `base_url`.
fn command(root: &Path, base_url: &str, args: &[&str]) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_agent"));
    command
        .env_clear()
        .env("AGENT_WORKSPACE", root.join("real-workspace"))
        .env("AGENT_PROVIDER", "openai")
        .env("AGENT_BASE_URL", base_url)
        .env("AGENT_MODEL", "mock")
        .env("AGENT_STREAM", "false")
        .env("AGENT_MAX_RETRIES", "0")
        // Where the scratch workspaces go, so the test can see they are gone.
        .env("TMPDIR", root.join("tmp"))
        .current_dir(root)
        .arg("--no-color")
        .arg("eval")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    command
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// A suite directory with one fixture, an empty real workspace and a temp dir.
fn layout() -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    let fixture = root.path().join("fixtures/base");
    std::fs::create_dir_all(&fixture).unwrap();
    std::fs::write(fixture.join("README.md"), "# a fixture\n").unwrap();
    std::fs::create_dir_all(root.path().join("real-workspace")).unwrap();
    std::fs::create_dir_all(root.path().join("tmp")).unwrap();
    std::fs::write(root.path().join("suite.json"), SUITE).unwrap();
    root
}

#[tokio::test]
async fn a_suite_with_one_pass_and_one_failure_reports_half_and_exits_3() {
    let llm = MockLlmServer::start(vec![
        // write-hello: one write, then an answer.
        Response::tool_call(
            "call_1",
            "write_file",
            r#"{"path":"hello.txt","content":"hello eval\n"}"#,
        ),
        Response::assistant_text("Done."),
        // never-writes: answers without doing the work.
        Response::assistant_text("I would rather not."),
    ])
    .await;
    let root = layout();

    let output = eval(root.path(), &llm, &["suite.json", "--out", "out"]).await;
    let stdout = text(&output.stdout);
    let stderr = text(&output.stderr);

    assert_eq!(
        output.status.code(),
        Some(EXIT_TASKS_FAILED),
        "--- stdout\n{stdout}\n--- stderr\n{stderr}"
    );
    let report: Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|error| panic!("stdout must be the JSON report only ({error}):\n{stdout}"));

    let summary = &report["summary"];
    assert_eq!(summary["pass_rate"], 0.5, "{summary}");
    assert_eq!(summary["attempts"], 2);
    assert_eq!(summary["passed"], 1);
    // 2 iterations for the first task, 1 for the second.
    assert_eq!(summary["iterations"], 3, "{summary}");
    // The mock bills 100+20 per tool call and 150+30 per answer.
    assert_eq!(summary["tokens"]["total"], 120 + 180 + 180, "{summary}");

    let tasks = report["tasks"].as_array().expect("tasks");
    assert_eq!(tasks[0]["id"], "write-hello");
    assert_eq!(tasks[0]["pass_rate"], 1.0);
    let first = &tasks[0]["runs"][0];
    assert_eq!(first["passed"], true, "{first}");
    assert_eq!(first["finish_reason"], "completed");
    assert_eq!(first["tool_calls"], 1);
    assert_eq!(first["denials"], 0, "eval approves inside the scratch copy");

    assert_eq!(tasks[1]["id"], "never-writes");
    let second = &tasks[1]["runs"][0];
    assert_eq!(second["passed"], false, "{second}");
    assert_eq!(second["checks"][0]["passed"], false);
    assert!(
        second["checks"][0]["detail"]
            .as_str()
            .unwrap()
            .contains("does not exist"),
        "{second}"
    );

    let fingerprint = &report["fingerprint"];
    assert_eq!(fingerprint["model"], "mock", "{fingerprint}");
    assert_eq!(
        fingerprint["system_prompt_fnv1a"].as_str().map(str::len),
        Some(16),
        "{fingerprint}"
    );

    // Progress went to stderr, not into the report.
    assert!(stderr.contains("write-hello #1 PASS"), "{stderr}");
    assert!(stderr.contains("never-writes #1 FAIL"), "{stderr}");

    // One session log per attempt, in the output directory.
    let sessions: Vec<_> = std::fs::read_dir(root.path().join("out/sessions"))
        .expect("the session directory exists")
        .collect();
    assert_eq!(sessions.len(), 2, "{sessions:?}");

    // Nothing written outside the scratch copies.
    assert!(
        !root.path().join("fixtures/base/hello.txt").exists(),
        "the fixture must not be edited"
    );
    assert_eq!(
        std::fs::read_dir(root.path().join("real-workspace"))
            .unwrap()
            .count(),
        0,
        "the real workspace must not be touched"
    );
    assert_eq!(
        std::fs::read_dir(root.path().join("tmp")).unwrap().count(),
        0,
        "every scratch workspace must be removed"
    );
}

#[tokio::test]
async fn keep_leaves_the_scratch_copy_and_names_it_in_the_report() {
    let llm = MockLlmServer::start(vec![
        Response::tool_call(
            "call_1",
            "write_file",
            r#"{"path":"hello.txt","content":"hello eval\n"}"#,
        ),
        Response::assistant_text("Done."),
        Response::assistant_text("No."),
    ])
    .await;
    let root = layout();

    let output = eval(root.path(), &llm, &["suite.json", "--out", "out", "--keep"]).await;
    let report: Value = serde_json::from_str(&text(&output.stdout)).expect("JSON report");

    let kept = report["tasks"][0]["runs"][0]["scratch_dir"]
        .as_str()
        .expect("--keep reports the directory");
    assert_eq!(
        std::fs::read_to_string(Path::new(kept).join("hello.txt")).unwrap(),
        "hello eval\n",
        "the kept copy holds what the model wrote"
    );
}

#[tokio::test]
async fn an_invalid_suite_fails_before_the_model_is_asked_anything() {
    // No responses: a request would find nobody listening.
    let llm = MockLlmServer::start(Vec::new()).await;
    let root = layout();
    std::fs::write(
        root.path().join("suite.json"),
        r#"{"version": 1, "tasks": [{"id": "a", "fixture": "fixtures/base", "prompt": "p", "checks": []}]}"#,
    )
    .unwrap();

    let output = eval(root.path(), &llm, &["suite.json"]).await;
    let stderr = text(&output.stderr);

    assert_eq!(output.status.code(), Some(1), "{stderr}");
    assert!(stderr.contains("has no checks"), "{stderr}");
    assert!(
        output.stdout.is_empty(),
        "no report for a suite that never ran"
    );
    assert!(llm.requests().await.is_empty());
}

/// One task, for the tests that are about how the run ends rather than about
/// the numbers.
const ONE_TASK: &str = r#"{
  "version": 1,
  "tasks": [
    {
      "id": "write-hello",
      "fixture": "fixtures/base",
      "prompt": "Create hello.txt saying hello eval.",
      "checks": [{"type": "file_contains", "path": "hello.txt", "contains": "hello eval"}]
    }
  ]
}"#;

#[cfg(unix)]
#[tokio::test]
async fn a_link_in_a_later_tasks_fixture_fails_before_the_model_is_asked_anything() {
    let llm = MockLlmServer::start(Vec::new()).await;
    let root = layout();
    let linked = root.path().join("fixtures/linked");
    std::fs::create_dir_all(linked.join("src")).unwrap();
    std::os::unix::fs::symlink("/etc", linked.join("src/escape")).unwrap();
    std::fs::write(
        root.path().join("suite.json"),
        serde_json::json!({
            "version": 1,
            "tasks": [
                {"id": "write-hello", "fixture": "fixtures/base", "prompt": "p",
                 "checks": [{"type": "file_contains", "path": "a", "contains": "a"}]},
                {"id": "never-writes", "fixture": "fixtures/linked", "prompt": "p",
                 "checks": [{"type": "file_contains", "path": "a", "contains": "a"}]},
            ]
        })
        .to_string(),
    )
    .unwrap();

    let output = eval(root.path(), &llm, &["suite.json", "--out", "out"]).await;
    let stderr = text(&output.stderr);

    assert_eq!(output.status.code(), Some(1), "{stderr}");
    assert!(stderr.contains("never-writes"), "{stderr}");
    assert!(stderr.contains("symbolic link"), "{stderr}");
    assert!(
        output.stdout.is_empty(),
        "no report for a suite that never ran"
    );
    assert!(
        llm.requests().await.is_empty(),
        "the first task must not have been run either"
    );
}

#[tokio::test]
async fn a_provider_that_refuses_every_request_exits_1_with_the_report() {
    let llm = MockLlmServer::start(vec![Response::status(
        "HTTP/1.1 401 Unauthorized",
        r#"{"error":{"message":"invalid api key"}}"#,
    )])
    .await;
    let root = layout();
    std::fs::write(root.path().join("suite.json"), ONE_TASK).unwrap();

    let output = eval(root.path(), &llm, &["suite.json", "--out", "out"]).await;
    let stdout = text(&output.stdout);
    let stderr = text(&output.stderr);

    assert_eq!(
        output.status.code(),
        Some(1),
        "a run that could not reach the model is not a task failure\n--- stderr\n{stderr}"
    );
    let report: Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|error| panic!("the report is still printed ({error}):\n{stdout}"));
    let run = &report["tasks"][0]["runs"][0];
    assert_eq!(run["finish_reason"], "error", "{run}");
    assert!(run["error"].as_str().is_some(), "{run}");
}

#[tokio::test]
async fn ctrl_c_stops_the_suite_with_a_partial_report_and_no_scratch_left() {
    let model = RecordingServer::unresponsive().await;
    let root = layout();
    let base_url = format!("{}/v1", model.url());
    let child = command(root.path(), &base_url, &["suite.json", "--out", "out"])
        .spawn()
        .expect("start the agent binary");

    let deadline = Instant::now() + DEADLINE;
    while model.received().is_empty() {
        assert!(Instant::now() < deadline, "the model was never asked");
        tokio::time::sleep(Duration::from_millis(20)).await;
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
        .unwrap_or_else(|_| panic!("eval was still running after {DEADLINE:?}"))
        .expect("wait for the agent");
    let stdout = text(&output.stdout);
    let stderr = text(&output.stderr);

    assert_eq!(output.status.code(), Some(130), "--- stderr\n{stderr}");
    let report: Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|error| panic!("a partial report is printed ({error}):\n{stdout}"));
    assert_eq!(report["interrupted"], true, "{report}");
    assert_eq!(report["summary"]["attempts"], 1, "{report}");
    let run = &report["tasks"][0]["runs"][0];
    assert_eq!(run["finish_reason"], "interrupted", "{run}");
    assert_eq!(run["passed"], false, "{run}");
    assert_eq!(
        report["tasks"][1]["runs"].as_array().map(Vec::len),
        Some(0),
        "the suite stops after the interrupted attempt: {report}"
    );
    assert_eq!(
        std::fs::read_dir(root.path().join("tmp")).unwrap().count(),
        0,
        "the interrupted attempt's scratch workspace must be removed"
    );
}
