//! Ctrl-C through the real binary.
//!
//! The state machine and the loop's interrupt handling are tested without
//! signals; what only a real process can show is the wiring: that `run` and
//! `chat` install the handler, that a SIGINT reaches the turn instead of
//! killing the process, and which exit code comes out.
//!
//! The model is a server that records the request and never answers, so the
//! turn is reliably mid-request when the signal is sent. "Mid-request" is
//! observed, not timed: the test waits until the request has arrived.

use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use agent_test_support::RecordingServer;
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, ChildStdin, Command};

/// Only here so a hang fails the test instead of the suite.
const DEADLINE: Duration = Duration::from_secs(60);

struct Running {
    child: Child,
    stdin: Option<ChildStdin>,
    stderr: Arc<Mutex<String>>,
    _workspace: TempDir,
    workspace: std::path::PathBuf,
}

/// Starts `agent <args>` against a model that never answers.
fn start(model: &RecordingServer, args: &[&str], stdin: bool) -> Running {
    let workspace = tempfile::tempdir().expect("a temporary workspace");
    let base_url = format!("{}/v1", model.url());
    let mut command = Command::new(env!("CARGO_BIN_EXE_agent"));
    command
        .env_clear()
        .env("AGENT_WORKSPACE", workspace.path())
        .env("AGENT_PROVIDER", "openai")
        .env("AGENT_BASE_URL", &base_url)
        .env("AGENT_MODEL", "mock")
        .env("AGENT_STREAM", "false")
        .env("AGENT_MAX_RETRIES", "0")
        .current_dir(workspace.path())
        .arg("--no-color")
        .args(args)
        .stdin(if stdin { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let mut child = command.spawn().expect("start the agent binary");
    let stdin = child.stdin.take();
    let stderr = Arc::new(Mutex::new(String::new()));
    let mut pipe = child.stderr.take().expect("piped stderr");
    let sink = stderr.clone();
    tokio::spawn(async move {
        let mut chunk = [0_u8; 4096];
        while let Ok(read) = pipe.read(&mut chunk).await {
            if read == 0 {
                break;
            }
            sink.lock()
                .unwrap()
                .push_str(&String::from_utf8_lossy(&chunk[..read]));
        }
    });

    Running {
        child,
        stdin,
        stderr,
        workspace: workspace.path().to_path_buf(),
        _workspace: workspace,
    }
}

impl Running {
    fn stderr(&self) -> String {
        self.stderr.lock().unwrap().clone()
    }

    /// Polls until `ready` holds - a bounded wait on observable state, not a
    /// guess at how long something takes.
    async fn until(&self, what: &str, ready: impl Fn(&Self) -> bool) {
        let deadline = Instant::now() + DEADLINE;
        while !ready(self) {
            assert!(
                Instant::now() < deadline,
                "never saw {what}\n--- stderr\n{}",
                self.stderr()
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// Ctrl-C, as the terminal would deliver it.
    fn interrupt(&self) {
        let pid = self.child.id().expect("still running");
        let status = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(format!("kill -INT {pid}"))
            .status()
            .expect("run kill");
        assert!(status.success(), "kill -INT {pid} failed");
    }

    async fn type_line(&mut self, line: &str) {
        let stdin = self.stdin.as_mut().expect("piped stdin");
        stdin.write_all(line.as_bytes()).await.expect("type");
        stdin.flush().await.expect("flush");
    }

    async fn exit_code(mut self) -> (Option<i32>, String) {
        let status = tokio::time::timeout(DEADLINE, self.child.wait())
            .await
            .unwrap_or_else(|_| panic!("still running after {DEADLINE:?}\n{}", self.stderr()))
            .expect("wait for the agent");
        // The stderr pump finishes once the pipe closes; give it the chance.
        let deadline = Instant::now() + DEADLINE;
        while Arc::strong_count(&self.stderr) > 1 && Instant::now() < deadline {
            tokio::task::yield_now().await;
        }
        (status.code(), self.stderr())
    }

    /// Every `.jsonl` record in the workspace's session directory.
    fn session_records(&self) -> String {
        let dir = self.workspace.join(".agent/sessions");
        let mut all = String::new();
        for entry in std::fs::read_dir(&dir).expect("a session directory") {
            let path = entry.unwrap().path();
            if path.extension().is_some_and(|ext| ext == "jsonl") {
                all.push_str(&std::fs::read_to_string(path).unwrap());
            }
        }
        all
    }
}

fn prompts_shown(stderr: &str) -> usize {
    stderr.matches("› ").count()
}

#[tokio::test]
async fn run_interrupted_mid_request_exits_130() {
    let model = RecordingServer::unresponsive().await;
    let agent = start(&model, &["run", "refactor everything"], false);
    agent
        .until("the model request", |_| !model.received().is_empty())
        .await;

    agent.interrupt();
    let (code, stderr) = agent.exit_code().await;

    assert_eq!(code, Some(130), "--- stderr\n{stderr}");
    assert!(stderr.contains("interrupted"), "{stderr}");
    assert!(
        !stderr.contains("error:"),
        "an interrupt is not an error: {stderr}"
    );
}

#[tokio::test]
async fn chat_interrupted_mid_turn_returns_to_the_prompt_and_keeps_the_conversation() {
    let model = RecordingServer::unresponsive().await;
    let mut agent = start(&model, &["chat"], true);
    agent.type_line("refactor everything\n").await;
    agent
        .until("the model request", |_| !model.received().is_empty())
        .await;

    agent.interrupt();
    agent
        .until("the prompt again", |agent| {
            prompts_shown(&agent.stderr()) >= 2
        })
        .await;
    let records = agent.session_records();
    agent.type_line("/exit\n").await;
    let (code, stderr) = agent.exit_code().await;

    assert_eq!(code, Some(0), "--- stderr\n{stderr}");
    assert!(stderr.contains("interrupted"), "{stderr}");
    assert!(!stderr.contains("error:"), "{stderr}");
    assert!(
        records.contains("refactor everything"),
        "the interrupted turn is still in the record: {records}"
    );
}

#[tokio::test]
async fn ctrl_c_at_the_chat_prompt_quits_with_success() {
    let model = RecordingServer::unresponsive().await;
    // stdin stays open, so the exit cannot be an end of input.
    let agent = start(&model, &["chat"], true);
    agent
        .until("the prompt", |agent| prompts_shown(&agent.stderr()) >= 1)
        .await;

    agent.interrupt();
    let (code, stderr) = agent.exit_code().await;

    assert_eq!(code, Some(0), "--- stderr\n{stderr}");
    assert!(
        model.received().is_empty(),
        "nothing was asked of the model"
    );
}
