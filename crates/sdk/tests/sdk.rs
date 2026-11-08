//! The SDK through its public API only, against a loopback mock model.
//!
//! Nothing here reaches into `agent_sdk::unstable` or the inner crates: if an
//! embedder cannot do it with what these tests use, the stable API is
//! missing something.

use std::path::Path;
use std::sync::{Arc, Mutex};

use agent_sdk::approval::{self, ApprovalDecision};
use agent_sdk::events::{EventDto, Versioned};
use agent_sdk::{Agent, AgentEvent, Config, EventSink, Interrupt, Reason};
use agent_test_support::{MockLlmServer, Response};
use serde_json::Value;
use tempfile::TempDir;

/// Configuration for a temporary workspace and the mock at `llm`, given as
/// pairs - the process environment is never touched.
fn config(workspace: &Path, llm: &MockLlmServer) -> Config {
    Config::from_vars([
        ("AGENT_WORKSPACE", workspace.display().to_string()),
        ("AGENT_PROVIDER", "openai".to_string()),
        ("AGENT_BASE_URL", llm.base_url().to_string()),
        ("AGENT_MODEL", "mock".to_string()),
        ("AGENT_STREAM", "false".to_string()),
        ("AGENT_MAX_RETRIES", "0".to_string()),
    ])
    .expect("valid configuration")
}

fn workspace() -> TempDir {
    tempfile::tempdir().expect("a temporary workspace")
}

/// Every event, as the published schema renders it.
#[derive(Default)]
struct JsonEvents(Mutex<Vec<Value>>);

impl JsonEvents {
    fn lines(&self) -> Vec<Value> {
        self.0.lock().unwrap().clone()
    }

    fn types(&self) -> Vec<String> {
        self.lines()
            .iter()
            .map(|line| line["type"].as_str().unwrap_or_default().to_string())
            .collect()
    }
}

impl EventSink for JsonEvents {
    fn emit(&self, event: AgentEvent) {
        let line = serde_json::to_value(Versioned::new(EventDto::from(&event))).unwrap();
        self.0.lock().unwrap().push(line);
    }
}

/// The `tool` message the model was sent for `call_id`, from a request body.
fn tool_result(request: &Value, call_id: &str) -> Option<String> {
    request["messages"]
        .as_array()?
        .iter()
        .find(|message| message["role"] == "tool" && message["tool_call_id"] == call_id)
        .and_then(|message| message["content"].as_str().map(str::to_string))
}

const WRITE: &str = r#"{"path":"note.txt","content":"hello"}"#;

#[tokio::test]
async fn run_returns_the_answer_with_how_the_turn_went() {
    let llm = MockLlmServer::start(vec![Response::assistant_text("Hello there.")]).await;
    let dir = workspace();
    let agent = Agent::builder(config(dir.path(), &llm))
        .build()
        .await
        .unwrap();

    let outcome = agent.run("Say hello").await.unwrap();

    assert_eq!(outcome.final_text, "Hello there.");
    assert_eq!(outcome.reason, Reason::Completed);
    assert!(outcome.is_complete());
    assert_eq!(outcome.iterations, 1);
    assert_eq!(outcome.usage.input_tokens, 150);
    assert_eq!(outcome.usage.output_tokens, 30);
    assert!(!outcome.session_id.is_empty());
    assert!(
        agent.tool_names().iter().any(|name| name == "read_file"),
        "the binary's tools are offered: {:?}",
        agent.tool_names()
    );
}

#[tokio::test]
async fn the_approval_callback_is_asked_and_its_denial_reaches_the_model_as_a_tool_result() {
    let llm = MockLlmServer::start(vec![
        Response::tool_call("c1", "write_file", WRITE),
        Response::assistant_text("Understood, I will not write it."),
    ])
    .await;
    let dir = workspace();
    let asked = Arc::new(Mutex::new(Vec::new()));
    let seen = asked.clone();
    let agent = Agent::builder(config(dir.path(), &llm))
        .approval(approval::from_fn(move |request| {
            seen.lock().unwrap().push(request.call.name.to_string());
            ApprovalDecision::Deny {
                reason: "no writes today".into(),
            }
        }))
        .build()
        .await
        .unwrap();

    let outcome = agent.run("Write a note").await.unwrap();

    assert_eq!(*asked.lock().unwrap(), ["write_file"]);
    assert_eq!(
        outcome.reason,
        Reason::Completed,
        "a denial is not a failure: the turn went on"
    );
    assert!(
        !dir.path().join("note.txt").exists(),
        "a denied write must not happen"
    );
    let requests = llm.json_requests().await;
    let answer = tool_result(&requests[1], "c1").expect("the call was answered");
    assert!(answer.contains("no writes today"), "got: {answer}");
}

#[tokio::test]
async fn without_a_callback_a_write_is_denied_and_a_read_is_not() {
    let llm = MockLlmServer::start(vec![
        Response::tool_call("c1", "write_file", WRITE),
        Response::tool_call("c2", "list_directory", r#"{"path":"."}"#),
        Response::assistant_text("Done."),
    ])
    .await;
    let dir = workspace();
    let agent = Agent::builder(config(dir.path(), &llm))
        .build()
        .await
        .unwrap();

    let outcome = agent.run("Write, then look").await.unwrap();

    assert_eq!(outcome.reason, Reason::Completed);
    assert!(!dir.path().join("note.txt").exists());
    let requests = llm.json_requests().await;
    let denied = tool_result(&requests[1], "c1").unwrap();
    assert!(denied.contains("declined"), "got: {denied}");
    let listed = tool_result(&requests[2], "c2").unwrap();
    assert!(
        !listed.contains("declined"),
        "read-only calls are approved by the default policy: {listed}"
    );
}

#[tokio::test]
async fn events_reach_the_sink_and_render_in_the_published_schema() {
    let llm = MockLlmServer::start(vec![
        Response::tool_call("c1", "list_directory", r#"{"path":"."}"#),
        Response::assistant_text("Nothing here."),
    ])
    .await;
    let dir = workspace();
    let events = Arc::new(JsonEvents::default());
    let agent = Agent::builder(config(dir.path(), &llm))
        .events(events.clone())
        .build()
        .await
        .unwrap();

    let outcome = agent.run("What is here?").await.unwrap();

    let types = events.types();
    for expected in [
        "run_started",
        "iteration_started",
        "model_responded",
        "tool_call_started",
        "tool_call_finished",
        "assistant_message",
        "run_finished",
    ] {
        assert!(
            types.iter().any(|t| t == expected),
            "no {expected}: {types:?}"
        );
    }
    let lines = events.lines();
    assert!(lines.iter().all(|line| line["v"] == 1), "{lines:?}");
    let started = &lines[0];
    assert_eq!(started["type"], "run_started");
    assert_eq!(started["session_id"], outcome.session_id.as_str());
    let call = lines
        .iter()
        .find(|line| line["type"] == "tool_call_started")
        .unwrap();
    assert_eq!(call["name"], "list_directory");
    assert_eq!(call["safety"], "read_only");
    let finished = lines.last().unwrap();
    assert_eq!(finished["type"], "run_finished");
    assert_eq!(finished["reason"], "completed");
}

#[tokio::test]
async fn an_interrupt_ends_the_turn_and_the_session_carries_on() {
    let llm = MockLlmServer::start(vec![
        Response::tool_call("c1", "write_file", WRITE),
        Response::assistant_text("Back again."),
    ])
    .await;
    let dir = workspace();
    let interrupt = Interrupt::new();
    // Fired from inside the turn, while a call is being decided: no timing
    // involved, and the turn is certainly running when it fires.
    let switch = interrupt.clone();
    let agent = Agent::builder(config(dir.path(), &llm))
        .approval(approval::from_fn(move |_| {
            switch.fire();
            ApprovalDecision::Deny {
                reason: "stopping".into(),
            }
        }))
        .build()
        .await
        .unwrap();

    let mut session = agent.session();
    let stopped = session
        .send_until("Write a note", &interrupt)
        .await
        .unwrap();
    assert_eq!(stopped.reason, Reason::Interrupted);
    assert!(interrupt.is_fired());

    let resumed = session.send("Are you there?").await.unwrap();
    assert_eq!(resumed.reason, Reason::Completed);
    assert_eq!(resumed.final_text, "Back again.");
    assert_eq!(resumed.session_id, stopped.session_id);
    assert_eq!(
        session.usage().input_tokens,
        100 + 150,
        "usage accumulates over the session's turns"
    );

    let requests = llm.json_requests().await;
    assert_eq!(requests.len(), 2, "the interrupted turn asked nothing more");
    assert!(
        tool_result(&requests[1], "c1").is_some(),
        "the interrupted call still has its result in the history"
    );
}

#[tokio::test]
async fn a_workspace_that_does_not_exist_fails_the_build_with_the_path() {
    let llm = MockLlmServer::start(Vec::new()).await;
    let config = config(Path::new("/"), &llm).with_workspace("/nonexistent/agent-sdk-test");

    let error = Agent::builder(config)
        .build()
        .await
        .err()
        .expect("no agent");

    let message = error.to_string();
    assert!(
        message.contains("/nonexistent/agent-sdk-test"),
        "got: {message}"
    );
}
