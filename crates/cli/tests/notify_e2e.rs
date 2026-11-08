//! Push notifications through the real binary.
//!
//! `end_to_end.rs` rebuilds the wiring by hand, so it cannot see a mistake in
//! the composition root, in `commands/run.rs` / `chat.rs`, or in how `main`
//! shuts the runtime down. These tests start `agent` itself as a child
//! process, pointed at two loopback servers: a scripted model and a webhook
//! receiver that records whatever arrives. Nothing leaves the machine.
//!
//! The child gets a cleared environment (no `std::env::set_var` anywhere) and
//! runs in a fresh temporary directory, so neither the developer's `.env` nor
//! their shell leaks into a result.
//!
//! "Delivered before exit" needs no polling: the receiver records a request
//! before answering it, and `agent run` waits for that answer before exiting.
//! So once the child has exited, a delivered notification is already in the
//! record - and a test that finds it missing has found a real bug.

use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};

use agent_test_support::{MockLlmServer, RecordedRequest, RecordingServer, Response};
use serde_json::Value;
use tempfile::TempDir;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

/// Far above anything these runs take - the slowest waits out the webhook
/// adapter's 5 s timeout. Only here so a hang fails the test instead of the
/// suite.
const DEADLINE: Duration = Duration::from_secs(60);

/// Distinctive enough that finding it in a body or on stderr cannot be a
/// coincidence.
const SECRET_PATH: &str = "hooks/T0SECRET/B0SECRET/xoxSECRETTOKEN";

/// What the child left behind.
struct Finished {
    code: Option<i32>,
    stdout: String,
    stderr: String,
    elapsed: Duration,
}

/// One `agent` invocation, built up before it is started.
struct Agent {
    workspace: TempDir,
    env: Vec<(String, String)>,
    args: Vec<String>,
    stdin: Option<String>,
}

impl Agent {
    /// `agent run <prompt>` against `llm`, with notifications not yet
    /// configured and the completion threshold at zero so a fast mock turn
    /// counts.
    fn run(llm: &MockLlmServer, prompt: &str) -> Self {
        let workspace = tempfile::tempdir().expect("a temporary workspace");
        let env = [
            ("AGENT_WORKSPACE", workspace.path().to_str().unwrap()),
            ("AGENT_BASE_URL", llm.base_url()),
            ("AGENT_MODEL", "mock"),
            // The mock replies with whole JSON bodies, not event streams.
            ("AGENT_STREAM", "false"),
            // A retry would ask the scripted model for a reply it does not
            // have, and back off before doing so.
            ("AGENT_MAX_RETRIES", "0"),
            ("AGENT_NOTIFY_MIN_SECS", "0"),
        ]
        .into_iter()
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .collect();
        Self {
            workspace,
            env,
            args: vec!["--no-color".into(), "run".into(), prompt.into()],
            stdin: None,
        }
    }

    /// `agent chat`, reading `stdin` as the human's typing.
    fn chat(llm: &MockLlmServer, stdin: &str) -> Self {
        let mut agent = Self::run(llm, "unused");
        agent.args = vec!["--no-color".into(), "chat".into()];
        agent.stdin = Some(stdin.to_string());
        agent
    }

    fn env(mut self, key: &str, value: &str) -> Self {
        self.env.push((key.to_string(), value.to_string()));
        self
    }

    /// Sends notifications to `url` in `format`.
    fn webhook(self, url: &str, format: &str) -> Self {
        self.env("AGENT_NOTIFY_WEBHOOK_URL", url)
            .env("AGENT_NOTIFY_FORMAT", format)
    }

    fn workspace(&self) -> &Path {
        self.workspace.path()
    }

    async fn finish(&self) -> Finished {
        let mut command = Command::new(env!("CARGO_BIN_EXE_agent"));
        command
            .env_clear()
            .envs(self.env.iter().map(|(key, value)| (key, value)))
            // `main` loads `.env` from here and its parents.
            .current_dir(self.workspace.path())
            .args(&self.args)
            .stdin(if self.stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let started = Instant::now();
        let mut child = command.spawn().expect("start the agent binary");
        if let Some(input) = &self.stdin {
            let mut stdin = child.stdin.take().expect("piped stdin");
            stdin.write_all(input.as_bytes()).await.expect("type");
            // Dropping it is the Ctrl-D that ends the session.
        }
        let output = tokio::time::timeout(DEADLINE, child.wait_with_output())
            .await
            .unwrap_or_else(|_| panic!("the agent was still running after {DEADLINE:?}"))
            .expect("wait for the agent");

        Finished {
            code: output.status.code(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            elapsed: started.elapsed(),
        }
    }
}

impl Finished {
    fn assert_code(&self, expected: i32) {
        assert_eq!(
            self.code,
            Some(expected),
            "exit code\n--- stdout\n{}\n--- stderr\n{}",
            self.stdout,
            self.stderr
        );
    }
}

/// A receiver that accepts everything.
async fn receiver() -> RecordingServer {
    RecordingServer::start(Response::ok("")).await
}

/// The one request the receiver should have got.
fn only(received: &[RecordedRequest]) -> &RecordedRequest {
    assert_eq!(
        received.len(),
        1,
        "expected exactly one notification, got {received:#?}"
    );
    &received[0]
}

fn as_str<'a>(body: &'a Value, field: &str) -> &'a str {
    body[field]
        .as_str()
        .unwrap_or_else(|| panic!("`{field}` is not a string in {body}"))
}

/// The answer a scripted model gives, long enough that the default 100-char
/// excerpt has to cut it.
const ANSWER: &str = "The build is green and every crate compiles cleanly. \
                      Beyond the first hundred characters lies text that must stay home.";

// --- agent run: completion -------------------------------------------------

#[tokio::test]
async fn a_completed_run_delivers_one_notification_quoting_the_answer_before_it_exits() {
    let llm = MockLlmServer::start(vec![Response::assistant_text(ANSWER)]).await;
    let hook = receiver().await;
    let agent = Agent::run(&llm, "is the build green?")
        .webhook(&format!("{}/{SECRET_PATH}", hook.url()), "json");

    let finished = agent.finish().await;

    finished.assert_code(0);
    let request = only(&hook.received()).clone();
    assert_eq!(request.method, "POST");
    assert_eq!(
        request.path,
        format!("/{SECRET_PATH}"),
        "json posts as given"
    );
    let body = request.json();
    assert_eq!(as_str(&body, "kind"), "completed", "{body}");
    let message = as_str(&body, "message");
    assert!(
        message.contains("The build is green and every crate compiles cleanly."),
        "the opening of the answer is missing: {message}"
    );
    assert!(
        !message.contains("must stay home"),
        "more than the 100-character default left the machine: {message}"
    );
}

#[tokio::test]
async fn stdout_carries_the_answer_and_nothing_about_notifications() {
    let llm = MockLlmServer::start(vec![Response::assistant_text(ANSWER)]).await;
    let hook = receiver().await;
    let agent = Agent::run(&llm, "is the build green?").webhook(hook.url(), "json");

    let finished = agent.finish().await;

    finished.assert_code(0);
    assert_eq!(
        finished.stdout,
        format!("{ANSWER}\n"),
        "`agent run > answer.md` must get the answer alone"
    );
}

#[tokio::test]
async fn the_notification_names_the_session_that_was_logged() {
    // `begin_turn` is how the session id reaches the notifier; a run that
    // forgot it would send an empty id.
    let llm = MockLlmServer::start(vec![Response::assistant_text("ok")]).await;
    let hook = receiver().await;
    let agent = Agent::run(&llm, "hi").webhook(hook.url(), "json");

    agent.finish().await.assert_code(0);

    let body = only(&hook.received()).json();
    let session = as_str(&body, "session_id");
    assert!(!session.is_empty(), "no session id: {body}");
    let log = agent
        .workspace()
        .join(".agent/sessions")
        .join(format!("{session}.jsonl"));
    assert!(
        log.is_file(),
        "the notified session {session} has no log at {}",
        log.display()
    );
}

#[tokio::test]
async fn the_excerpt_setting_reaches_the_notification() {
    let llm = MockLlmServer::start(vec![Response::assistant_text(ANSWER)]).await;
    let hook = receiver().await;
    let agent = Agent::run(&llm, "hi")
        .webhook(hook.url(), "json")
        .env("AGENT_NOTIFY_EXCERPT_CHARS", "9");

    agent.finish().await.assert_code(0);

    let body = only(&hook.received()).json();
    let message = as_str(&body, "message");
    assert!(message.contains("The build…"), "{message}");
    assert!(!message.contains("is green"), "{message}");
}

#[tokio::test]
async fn a_zero_excerpt_keeps_the_answer_out_of_the_notification() {
    let llm = MockLlmServer::start(vec![Response::assistant_text(ANSWER)]).await;
    let hook = receiver().await;
    let agent = Agent::run(&llm, "hi")
        .webhook(hook.url(), "json")
        .env("AGENT_NOTIFY_EXCERPT_CHARS", "0");

    agent.finish().await.assert_code(0);

    let body = only(&hook.received()).json();
    assert_eq!(as_str(&body, "kind"), "completed", "{body}");
    assert!(
        !body.to_string().contains("build"),
        "the answer was sent anyway: {body}"
    );
}

// --- agent run: failures ---------------------------------------------------

#[tokio::test]
async fn a_provider_failure_is_notified_without_the_error_text_and_exits_as_before() {
    let llm = MockLlmServer::start(vec![Response::status(
        "500 Internal Server Error",
        r#"{"error":{"message":"upstream exploded: LEAKED-RESPONSE-BODY"}}"#,
    )])
    .await;
    let hook = receiver().await;
    let agent = Agent::run(&llm, "hi").webhook(hook.url(), "json");

    let finished = agent.finish().await;

    finished.assert_code(1);
    assert!(
        finished.stderr.contains("LEAKED-RESPONSE-BODY"),
        "the terminal still shows the full error: {}",
        finished.stderr
    );
    assert_eq!(finished.stdout, "", "no answer, so nothing on stdout");

    let request = only(&hook.received()).clone();
    let body = request.json();
    assert_eq!(as_str(&body, "kind"), "failed", "{body}");
    assert!(
        as_str(&body, "message").contains("HTTP 500"),
        "the coarse reason is missing: {body}"
    );
    let llm_address = llm
        .base_url()
        .trim_start_matches("http://")
        .trim_end_matches("/v1");
    for leaked in [
        "LEAKED-RESPONSE-BODY",
        "upstream exploded",
        llm_address,
        "/v1",
    ] {
        assert!(
            !request.body.contains(leaked),
            "`{leaked}` reached the webhook: {}",
            request.body
        );
    }
}

#[tokio::test]
async fn running_out_of_iterations_is_a_failure_notification_and_exit_code_2() {
    let llm = MockLlmServer::start(vec![Response::tool_call(
        "call_1",
        "read_file",
        r#"{"path":"missing.txt"}"#,
    )])
    .await;
    let hook = receiver().await;
    let agent = Agent::run(&llm, "hi")
        .webhook(hook.url(), "json")
        .env("AGENT_MAX_ITERATIONS", "1");

    let finished = agent.finish().await;

    finished.assert_code(2);
    let body = only(&hook.received()).json();
    assert_eq!(as_str(&body, "kind"), "failed", "{body}");
    assert!(
        as_str(&body, "message").contains("iteration limit (1)"),
        "{body}"
    );
}

// --- disabled and filtered -------------------------------------------------

#[tokio::test]
async fn without_a_url_nothing_is_sent_and_the_run_is_unchanged() {
    let llm = MockLlmServer::start(vec![Response::assistant_text(ANSWER)]).await;
    let hook = receiver().await;
    // The receiver exists but nobody is told about it, and every other
    // notification setting is present - only the URL switches sending on.
    let agent = Agent::run(&llm, "hi")
        .env("AGENT_NOTIFY_FORMAT", "json")
        .env("AGENT_NOTIFY_EVENTS", "approval,completed,failed");

    let finished = agent.finish().await;

    finished.assert_code(0);
    assert_eq!(finished.stdout, format!("{ANSWER}\n"));
    assert!(
        !finished.stderr.contains("notification"),
        "a disabled notifier has nothing to warn about: {}",
        finished.stderr
    );
    assert!(hook.received().is_empty(), "{:#?}", hook.received());
}

#[tokio::test]
async fn an_unsubscribed_kind_is_not_sent() {
    let llm = MockLlmServer::start(vec![Response::assistant_text("ok")]).await;
    let hook = receiver().await;
    let agent = Agent::run(&llm, "hi")
        .webhook(hook.url(), "json")
        .env("AGENT_NOTIFY_EVENTS", "failed");

    agent.finish().await.assert_code(0);

    assert!(hook.received().is_empty(), "{:#?}", hook.received());
}

#[tokio::test]
async fn a_subscribed_failure_is_sent_when_completions_are_not_wanted() {
    let llm = MockLlmServer::start(vec![Response::status("500 Internal Server Error", "{}")]).await;
    let hook = receiver().await;
    let agent = Agent::run(&llm, "hi")
        .webhook(hook.url(), "json")
        .env("AGENT_NOTIFY_EVENTS", "failed");

    agent.finish().await.assert_code(1);

    let body = only(&hook.received()).json();
    assert_eq!(as_str(&body, "kind"), "failed", "{body}");
}

#[tokio::test]
async fn a_high_threshold_holds_back_a_quick_completion() {
    let llm = MockLlmServer::start(vec![Response::assistant_text("ok")]).await;
    let hook = receiver().await;
    let agent = Agent::run(&llm, "hi")
        .webhook(hook.url(), "json")
        .env("AGENT_NOTIFY_MIN_SECS", "3600");

    agent.finish().await.assert_code(0);

    assert!(hook.received().is_empty(), "{:#?}", hook.received());
}

#[tokio::test]
async fn a_high_threshold_does_not_hold_back_a_failure() {
    let llm = MockLlmServer::start(vec![Response::status("500 Internal Server Error", "{}")]).await;
    let hook = receiver().await;
    let agent = Agent::run(&llm, "hi")
        .webhook(hook.url(), "json")
        .env("AGENT_NOTIFY_MIN_SECS", "3600");

    agent.finish().await.assert_code(1);

    let body = only(&hook.received()).json();
    assert_eq!(as_str(&body, "kind"), "failed", "{body}");
}

// --- the receiver misbehaves -----------------------------------------------

/// Checks that a broken receiver changed nothing the caller of `agent run`
/// can observe, and that it was reported without the URL.
fn assert_unaffected_and_warned(finished: &Finished) {
    finished.assert_code(0);
    assert_eq!(finished.stdout, format!("{ANSWER}\n"));
    assert!(
        finished.stderr.contains("notification"),
        "the operator was not told: {}",
        finished.stderr
    );
    for leaked in ["SECRET", "T0SECRET", "hooks/"] {
        assert!(
            !finished.stderr.contains(leaked),
            "`{leaked}` from the webhook URL is on stderr: {}",
            finished.stderr
        );
    }
}

#[tokio::test]
async fn a_failing_receiver_changes_nothing_but_a_warning_without_the_url() {
    let llm = MockLlmServer::start(vec![Response::assistant_text(ANSWER)]).await;
    let hook = RecordingServer::start(Response::status("500 Internal Server Error", "")).await;
    let agent = Agent::run(&llm, "hi").webhook(&format!("{}/{SECRET_PATH}", hook.url()), "json");

    let finished = agent.finish().await;

    assert_unaffected_and_warned(&finished);
    assert_eq!(hook.received().len(), 1, "sent once, not retried");
}

#[tokio::test]
async fn an_unreachable_receiver_changes_nothing_but_a_warning_without_the_url() {
    let llm = MockLlmServer::start(vec![Response::assistant_text(ANSWER)]).await;
    // A port that was just listening and no longer is: connection refused.
    let closed = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap()
    };
    let agent = Agent::run(&llm, "hi").webhook(&format!("http://{closed}/{SECRET_PATH}"), "json");

    let finished = agent.finish().await;

    assert_unaffected_and_warned(&finished);
}

#[tokio::test]
async fn a_receiver_that_never_answers_delays_exit_by_a_bounded_time() {
    let llm = MockLlmServer::start(vec![Response::assistant_text(ANSWER)]).await;
    let hook = RecordingServer::unresponsive().await;
    let agent = Agent::run(&llm, "hi").webhook(&format!("{}/{SECRET_PATH}", hook.url()), "json");

    let finished = agent.finish().await;

    assert_unaffected_and_warned(&finished);
    assert_eq!(hook.received().len(), 1, "the request did go out");
    // Adapter timeout (5 s) under the drain bound (6 s), plus startup.
    assert!(
        finished.elapsed < Duration::from_secs(15),
        "an unresponsive webhook held the process for {:?}",
        finished.elapsed
    );
}

// --- approvals through the binary ------------------------------------------

#[tokio::test]
async fn a_child_without_a_terminal_never_pages_for_approval() {
    // stdin is not a TTY, so the default read-only policy denies the write
    // on its own. Nobody was asked, so nobody should be paged.
    let llm = MockLlmServer::start(vec![
        Response::tool_call("call_1", "write_file", r#"{"path":"a.txt","content":"x"}"#),
        Response::assistant_text("I could not write it."),
    ])
    .await;
    let hook = receiver().await;
    let agent = Agent::run(&llm, "write a.txt").webhook(hook.url(), "json");

    let finished = agent.finish().await;

    finished.assert_code(0);
    assert!(
        !agent.workspace().join("a.txt").exists(),
        "the write should have been denied"
    );
    let received = hook.received();
    let kinds: Vec<String> = received
        .iter()
        .map(|request| as_str(&request.json(), "kind").to_string())
        .collect();
    assert_eq!(kinds, ["completed"], "{received:#?}");
}

// --- agent chat --------------------------------------------------------------

#[tokio::test]
async fn chat_delivers_the_last_turns_notification_before_exiting_on_eof() {
    let llm = MockLlmServer::start(vec![Response::assistant_text(ANSWER)]).await;
    let hook = receiver().await;
    let agent = Agent::chat(&llm, "is the build green?\n").webhook(hook.url(), "json");

    let finished = agent.finish().await;

    finished.assert_code(0);
    let body = only(&hook.received()).json();
    assert_eq!(as_str(&body, "kind"), "completed", "{body}");
    assert!(
        as_str(&body, "message").contains("The build is green"),
        "{body}"
    );
}

#[tokio::test]
async fn a_failed_chat_turn_is_notified_and_the_session_carries_on() {
    let llm = MockLlmServer::start(vec![
        Response::status("500 Internal Server Error", "{}"),
        Response::assistant_text("second time lucky"),
    ])
    .await;
    let hook = receiver().await;
    let agent = Agent::chat(&llm, "first\nsecond\n/exit\n").webhook(hook.url(), "json");

    let finished = agent.finish().await;

    finished.assert_code(0);
    assert!(
        finished.stdout.contains("second time lucky"),
        "the second turn did not run: {}",
        finished.stderr
    );
    let kinds: Vec<String> = hook
        .received()
        .iter()
        .map(|request| as_str(&request.json(), "kind").to_string())
        .collect();
    assert_eq!(kinds, ["failed", "completed"]);
}

// --- wire formats through the binary ----------------------------------------

#[tokio::test]
async fn ntfy_publishes_to_the_server_root_with_the_topic_in_the_body() {
    let llm = MockLlmServer::start(vec![Response::assistant_text(ANSWER)]).await;
    let hook = receiver().await;
    let agent = Agent::run(&llm, "hi").webhook(&format!("{}/agent-topic_42", hook.url()), "ntfy");

    agent.finish().await.assert_code(0);

    let request = only(&hook.received()).clone();
    assert_eq!(request.path, "/", "the topic must not stay in the path");
    let body = request.json();
    assert_eq!(as_str(&body, "topic"), "agent-topic_42", "{body}");
    assert_eq!(as_str(&body, "title"), "agent: turn finished", "{body}");
    assert!(
        as_str(&body, "message").contains("The build is green"),
        "{body}"
    );
    assert_eq!(
        body["tags"],
        serde_json::json!(["white_check_mark"]),
        "{body}"
    );
}

#[tokio::test]
async fn slack_posts_text_to_the_url_as_given_without_unfurling() {
    let llm = MockLlmServer::start(vec![Response::assistant_text(ANSWER)]).await;
    let hook = receiver().await;
    let agent = Agent::run(&llm, "hi").webhook(&format!("{}/{SECRET_PATH}", hook.url()), "slack");

    agent.finish().await.assert_code(0);

    let request = only(&hook.received()).clone();
    assert_eq!(request.path, format!("/{SECRET_PATH}"));
    let body = request.json();
    let text = as_str(&body, "text");
    assert!(text.starts_with("*agent: turn finished*"), "{text}");
    assert!(text.contains("The build is green"), "{text}");
    assert_eq!(body["unfurl_links"], false, "{body}");
    assert_eq!(body["unfurl_media"], false, "{body}");
}
