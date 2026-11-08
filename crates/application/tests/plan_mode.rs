//! Plan mode through the loop: what the model is told, what it may run, and
//! what does not change in front of it.

mod common;

use std::sync::Arc;

use agent_application::agent::Session;
use agent_application::approval::{PlanMode, PlanModeGate};
use agent_domain::model::llm::ChatRequest;
use agent_domain::model::message::{ContentBlock, Role};
use agent_domain::ports::events::NullEventSink;
use serde_json::json;

use common::*;

fn plan_mode_notes(request: &ChatRequest) -> usize {
    request
        .messages
        .iter()
        .filter(|message| message.role == Role::User && message.text().contains("plan mode"))
        .count()
}

fn tool_results(request: &ChatRequest) -> Vec<(bool, String)> {
    request
        .messages
        .iter()
        .flat_map(|message| message.content.iter())
        .filter_map(|block| match block {
            ContentBlock::ToolResult(result) => Some((result.is_error, result.content.clone())),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn while_planning_the_model_reads_and_plans_but_cannot_write() {
    let file_system = MemoryFileSystem::with(&[("src/main.rs", "fn main() {}\n")]);
    let provider = ScriptedProvider::new(vec![
        tool_use("r1", "read_file", json!({"path": "src/main.rs"})),
        tool_use("t1", "todo_write", todos(&[("rewrite main", "pending")])),
        tool_use(
            "w1",
            "write_file",
            json!({"path": "src/main.rs", "content": "gone"}),
        ),
        assistant("Here is my plan."),
    ]);
    let mode = PlanMode::new();
    let agent = loop_with(
        provider.clone(),
        registry_with_plan(file_system.clone()),
        Arc::new(PlanModeGate::new(Arc::new(AlwaysApprove), mode.clone())),
        Arc::new(NullEventSink),
        10,
    );

    let mut session = Session::new("plan-mode");
    mode.set(&mut session, true);
    agent.run(&mut session, "rewrite main").await.unwrap();

    assert_eq!(
        file_system.get("src/main.rs").as_deref(),
        Some("fn main() {}\n"),
        "nothing was written"
    );
    let results = tool_results(provider.requests().last().unwrap());
    assert_eq!(results.len(), 3, "{results:?}");
    assert!(!results[0].0, "read_file ran: {results:?}");
    assert!(!results[1].0, "todo_write ran: {results:?}");
    assert!(results[2].0, "write_file was refused: {results:?}");
    assert!(
        results[2].1.contains("plan mode is on") && results[2].1.contains("Present your plan"),
        "the model is told why and what to do instead: {}",
        results[2].1
    );
}

#[tokio::test]
async fn a_switch_is_announced_once_and_changes_nothing_else_in_the_request() {
    let provider = ScriptedProvider::new(vec![
        assistant("first"),
        assistant("second"),
        assistant("third"),
    ]);
    let mode = PlanMode::new();
    let agent = loop_with(
        provider.clone(),
        registry_with_plan(MemoryFileSystem::with(&[])),
        Arc::new(PlanModeGate::new(Arc::new(AlwaysApprove), mode.clone())),
        Arc::new(NullEventSink),
        10,
    );
    let mut session = Session::new("plan-mode");

    agent.run(&mut session, "before").await.unwrap();
    mode.set(&mut session, true);
    agent.run(&mut session, "while planning").await.unwrap();
    agent.run(&mut session, "still planning").await.unwrap();

    let requests = provider.requests();
    assert_eq!(requests.len(), 3);
    assert_eq!(plan_mode_notes(&requests[0]), 0);
    assert_eq!(
        plan_mode_notes(&requests[1]),
        1,
        "announced with the next message"
    );
    assert_eq!(
        plan_mode_notes(&requests[2]),
        1,
        "and only that once: the history still holds it, no second one is added"
    );
    let last_user = requests[1]
        .messages
        .iter()
        .rev()
        .find(|message| message.role == Role::User)
        .unwrap()
        .text();
    assert!(
        last_user.starts_with("[Agent note") && last_user.ends_with("while planning"),
        "the note goes in front of the user's words: {last_user}"
    );

    let tools = |request: &ChatRequest| serde_json::to_vec(&request.tools).unwrap();
    assert_eq!(
        tools(&requests[0]),
        tools(&requests[1]),
        "the tool list is byte for byte the same"
    );
    assert_eq!(
        requests[0].system, requests[1].system,
        "the system prompt is too"
    );
}
