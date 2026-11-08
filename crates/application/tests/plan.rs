//! Keeping the model's work plan in front of it across shortenings, and
//! announcing it only when it changes.

mod common;

use std::sync::Arc;

use agent_application::agent::{AgentLoopConfig, Session};
use agent_domain::model::conversation::Conversation;
use agent_domain::model::llm::{ChatRequest, StopReason, TaskKind};
use agent_domain::model::message::{ContentBlock, Message, Role};
use agent_domain::model::plan::{Plan, PlanItem, PlanStatus};
use agent_domain::model::tool::{ToolCall, ToolCallId, ToolName, ToolResult};
use agent_domain::ports::events::{AgentEvent, FinishReason, NullEventSink};
use serde_json::json;

use common::*;

fn plan_of(steps: &[(&str, PlanStatus)]) -> Plan {
    Plan::new(
        steps
            .iter()
            .map(|(content, status)| PlanItem::new(*content, *status))
            .collect(),
    )
    .unwrap()
}

fn notes_in(request: &ChatRequest) -> usize {
    request
        .messages
        .iter()
        .filter(|message| message.role == Role::User && message.text().starts_with(NOTE))
        .count()
}

fn plan_events(events: &RecordingSink) -> Vec<Plan> {
    events
        .events
        .lock()
        .unwrap()
        .iter()
        .filter_map(|event| match event {
            AgentEvent::PlanUpdated { plan } => Some(plan.clone()),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn the_plan_survives_compaction_as_a_single_note() {
    let file_system = MemoryFileSystem::with(&[("src/main.rs", "fn main() {}\n")]);
    let provider = ScriptedProvider::new(vec![
        assistant("SUMMARY OF EARLIER WORK"),
        tool_use("r1", "read_file", json!({"path": "src/main.rs"})),
        assistant("Lexer done."),
    ]);
    let agent = loop_with_config(
        provider.clone(),
        registry_with_plan(file_system),
        Arc::new(AlwaysApprove),
        Arc::new(NullEventSink),
        compacting_config(8 * 1024),
    );

    let mut session = Session::resumed("plan-c", planned_record(UNFINISHED));
    agent.run(&mut session, "carry on").await.unwrap();

    let requests = provider.requests();
    assert_eq!(
        requests[0].metadata.task_kind,
        TaskKind::Summarize,
        "precondition: the history really was compacted"
    );
    let after = &requests[1];
    assert!(
        !after
            .messages
            .iter()
            .flat_map(Message::tool_calls)
            .any(|c| c.name.as_str() == "todo_write"),
        "precondition: the call that wrote the plan was folded away"
    );
    let note = after.messages.last().expect("a message").text();
    assert!(note.starts_with(NOTE), "the note comes last: {note}");
    assert!(note.contains("[>] write the lexer"), "{note}");
    assert!(note.contains("[ ] write the parser"), "{note}");
    assert!(note.contains("not a message from the user"), "{note}");

    assert_eq!(
        notes_in(&requests[2]),
        1,
        "the next iteration did not shorten anything, so no second note"
    );
}

#[tokio::test]
async fn the_plan_survives_trimming_without_a_summary() {
    let file_system = MemoryFileSystem::with(&[("src/main.rs", "fn main() {}\n")]);
    let provider = ScriptedProvider::new(vec![
        tool_use("r1", "read_file", json!({"path": "src/main.rs"})),
        assistant("Lexer done."),
    ]);
    let agent = loop_with_config(
        provider.clone(),
        registry_with_plan(file_system),
        Arc::new(AlwaysApprove),
        Arc::new(NullEventSink),
        AgentLoopConfig {
            compact: false,
            keep_recent_messages: 2,
            ..compacting_config(8 * 1024)
        },
    );

    let mut session = Session::resumed("plan-t", planned_record(UNFINISHED));
    agent.run(&mut session, "carry on").await.unwrap();

    let requests = provider.requests();
    assert!(
        requests
            .iter()
            .all(|r| r.metadata.task_kind == TaskKind::Agentic),
        "trimming alone, no summary request"
    );
    let first = &requests[0];
    assert!(
        !first
            .messages
            .iter()
            .flat_map(Message::tool_calls)
            .any(|c| c.name.as_str() == "todo_write"),
        "precondition: the call that wrote the plan was trimmed away"
    );
    assert_eq!(notes_in(first), 1);
    assert!(
        first
            .messages
            .last()
            .unwrap()
            .text()
            .contains("[>] write the lexer"),
        "the note comes last and carries the plan"
    );
    assert_eq!(notes_in(&requests[1]), 1, "pinned once, not per iteration");
    let sent: usize = first.messages.iter().map(Message::approx_bytes).sum();
    assert!(
        sent <= 8 * 1024,
        "room for the note was reserved inside the budget: {sent} bytes"
    );
}

/// When the messages trimming may not touch already exceed the budget, every
/// iteration trims a little more. The plan is still pinned once, not once per
/// trim.
#[tokio::test]
async fn a_history_that_stays_over_budget_is_not_given_a_note_per_iteration() {
    let file_system = MemoryFileSystem::with(&[("src/main.rs", "fn main() {}\n")]);
    let provider = ScriptedProvider::new(vec![
        tool_use("r1", "read_file", json!({"path": "src/main.rs"})),
        tool_use("r2", "read_file", json!({"path": "src/main.rs"})),
        assistant("Lexer done."),
    ]);
    let events = Arc::new(RecordingSink::default());
    let agent = loop_with_config(
        provider.clone(),
        registry_with_plan(file_system),
        Arc::new(AlwaysApprove),
        events.clone(),
        AgentLoopConfig {
            compact: false,
            keep_recent_messages: 6,
            max_iterations: 5,
            ..compacting_config(8 * 1024)
        },
    );

    let mut session = Session::resumed("plan-o", planned_record(UNFINISHED));
    agent.run(&mut session, "carry on").await.unwrap();

    let trims = events
        .events
        .lock()
        .unwrap()
        .iter()
        .filter(|event| matches!(event, AgentEvent::HistoryTrimmed { .. }))
        .count();
    assert!(
        trims >= 2,
        "precondition: more than one iteration trimmed, got {trims}"
    );
    for (index, request) in provider.requests().iter().enumerate() {
        assert_eq!(notes_in(request), 1, "request {index}");
    }
}

#[tokio::test]
async fn nothing_is_pinned_without_a_plan_or_once_every_step_is_done() {
    let finished = &[
        ("read the grammar", "completed"),
        ("write the lexer", "completed"),
    ];
    let cases = [
        (
            "no plan",
            Session::resumed("plan-n1", crowded_session("x").conversation),
        ),
        (
            "all done",
            Session::resumed("plan-n2", planned_record(finished)),
        ),
    ];

    for (label, mut session) in cases {
        let provider = ScriptedProvider::new(vec![assistant("ok")]);
        let agent = loop_with_config(
            provider.clone(),
            registry_with_plan(MemoryFileSystem::default().into()),
            Arc::new(AlwaysApprove),
            Arc::new(NullEventSink),
            AgentLoopConfig {
                compact: false,
                ..compacting_config(8 * 1024)
            },
        );
        agent.run(&mut session, "carry on").await.unwrap();

        let request = &provider.requests()[0];
        assert!(
            request.messages.len() < 14,
            "{label}: precondition: the history was trimmed"
        );
        assert_eq!(
            notes_in(request),
            0,
            "{label}: nothing to remind the model of"
        );
    }
}

#[tokio::test]
async fn plan_updates_are_announced_only_when_the_plan_changes() {
    let provider = ScriptedProvider::new(vec![
        tool_use(
            "p1",
            "todo_write",
            todos(&[("a", "in_progress"), ("b", "pending")]),
        ),
        tool_use(
            "p2",
            "todo_write",
            todos(&[("a", "in_progress"), ("b", "pending")]),
        ),
        tool_use(
            "p3",
            "todo_write",
            todos(&[("a", "completed"), ("b", "in_progress")]),
        ),
        tool_use("p4", "todo_write", todos(&[])),
        assistant("All done."),
    ]);
    let events = Arc::new(RecordingSink::default());
    let agent = loop_with(
        provider,
        registry_with_plan(MemoryFileSystem::default().into()),
        Arc::new(AlwaysApprove),
        events.clone(),
        10,
    );

    let mut session = Session::new("plan-e");
    agent.run(&mut session, "do a and b").await.unwrap();

    assert_eq!(
        plan_events(&events),
        [
            plan_of(&[("a", PlanStatus::InProgress), ("b", PlanStatus::Pending)]),
            plan_of(&[("a", PlanStatus::Completed), ("b", PlanStatus::InProgress)]),
            Plan::default(),
        ],
        "the repeated plan is not announced again; clearing is"
    );
    assert_eq!(session.plan(), None, "an empty list clears the plan");
}

#[tokio::test]
async fn the_last_plan_of_a_turn_wins_even_when_read_only_calls_run_in_parallel() {
    let file_system = MemoryFileSystem::with(&[("src/main.rs", "fn main() {}\n")]);
    let calls = [
        ("p1", "todo_write", todos(&[("first", "pending")])),
        ("r1", "read_file", json!({"path": "src/main.rs"})),
        ("p2", "todo_write", todos(&[("second", "in_progress")])),
    ]
    .into_iter()
    .map(|(id, tool, args)| {
        ContentBlock::ToolCall(ToolCall::new(
            ToolCallId::new(id),
            ToolName::new(tool).unwrap(),
            args,
        ))
    })
    .collect();
    let provider = ScriptedProvider::new(vec![
        response(Message::assistant(calls), StopReason::ToolUse),
        assistant("ok"),
    ]);
    let events = Arc::new(RecordingSink::default());
    let agent = loop_with_config(
        provider,
        registry_with_plan(file_system),
        Arc::new(AlwaysApprove),
        events.clone(),
        AgentLoopConfig {
            parallel_read_only_tools: true,
            ..AgentLoopConfig::default()
        },
    );

    let mut session = Session::new("plan-p");
    agent.run(&mut session, "go").await.unwrap();

    let second = plan_of(&[("second", PlanStatus::InProgress)]);
    assert_eq!(session.plan(), Some(&second));
    assert_eq!(plan_events(&events), [second], "one turn, one announcement");
}

#[tokio::test]
async fn an_invalid_plan_is_returned_to_the_model_and_the_turn_carries_on() {
    let provider = ScriptedProvider::new(vec![
        tool_use(
            "p1",
            "todo_write",
            todos(&[("a", "in_progress"), ("b", "in_progress")]),
        ),
        assistant("I will fix the plan."),
    ]);
    let events = Arc::new(RecordingSink::default());
    let agent = loop_with(
        provider.clone(),
        registry_with_plan(MemoryFileSystem::default().into()),
        Arc::new(AlwaysApprove),
        events.clone(),
        10,
    );

    let mut session = Session::new("plan-i");
    let outcome = agent.run(&mut session, "plan it").await.unwrap();

    assert_eq!(outcome.reason, FinishReason::Completed);
    let result = provider.requests()[1]
        .messages
        .iter()
        .flat_map(|message| message.content.iter())
        .find_map(|block| match block {
            ContentBlock::ToolResult(result) => Some(result.clone()),
            _ => None,
        })
        .expect("the call was answered");
    assert!(result.is_error);
    assert!(
        result.content.contains("only one step"),
        "{}",
        result.content
    );
    assert_eq!(
        session.plan(),
        None,
        "a refused plan never becomes the plan"
    );
    assert!(plan_events(&events).is_empty());
}

#[tokio::test]
async fn plan_updates_leave_the_system_prompt_and_tools_byte_identical() {
    let provider = ScriptedProvider::new(vec![
        tool_use(
            "p1",
            "todo_write",
            todos(&[("SECRET-STEP-ONE", "in_progress")]),
        ),
        assistant("Started."),
        tool_use(
            "p2",
            "todo_write",
            todos(&[("SECRET-STEP-ONE", "completed")]),
        ),
        assistant("Finished."),
    ]);
    let agent = loop_with(
        provider.clone(),
        registry_with_plan(MemoryFileSystem::default().into()),
        Arc::new(AlwaysApprove),
        Arc::new(NullEventSink),
        10,
    );

    let mut session = Session::new("plan-s");
    agent.run(&mut session, "start").await.unwrap();
    agent.run(&mut session, "finish").await.unwrap();

    let requests = provider.requests();
    assert_eq!(requests.len(), 4);
    let system = requests[0].system.clone().expect("a system prompt");
    assert!(
        !system.contains("SECRET-STEP-ONE"),
        "the plan stays out of the prompt"
    );
    for (index, request) in requests.iter().enumerate() {
        assert_eq!(
            request.system.as_deref(),
            Some(system.as_str()),
            "request {index}"
        );
        assert_eq!(request.tools, requests[0].tools, "request {index}");
        assert_eq!(
            notes_in(request),
            0,
            "request {index}: nothing was shortened"
        );
    }
}

#[test]
fn a_resumed_session_gets_back_the_last_plan_it_recorded() {
    let mut messages = planned_record(UNFINISHED).into_messages();
    // A later refusal must not count; a later success must.
    let refused = ToolCall::new(
        ToolCallId::new("plan-1"),
        ToolName::new("todo_write").unwrap(),
        todos(&[("refused", "pending")]),
    );
    messages.push(Message::assistant(vec![ContentBlock::ToolCall(
        refused.clone(),
    )]));
    messages.push(Message::tool_results(vec![ToolResult::error(
        &refused, "no",
    )]));

    let session = Session::resumed("plan-r", Conversation::from_messages(messages.clone()));
    assert_eq!(
        session.plan(),
        Some(&plan_of(&[
            ("read the grammar", PlanStatus::Completed),
            ("write the lexer", PlanStatus::InProgress),
            ("write the parser", PlanStatus::Pending),
        ]))
    );

    let cleared = ToolCall::new(
        ToolCallId::new("plan-2"),
        ToolName::new("todo_write").unwrap(),
        todos(&[]),
    );
    messages.push(Message::assistant(vec![ContentBlock::ToolCall(
        cleared.clone(),
    )]));
    messages.push(Message::tool_results(vec![ToolResult::ok(
        &cleared, "cleared",
    )]));
    let session = Session::resumed("plan-r2", Conversation::from_messages(messages));
    assert_eq!(
        session.plan(),
        None,
        "a plan cleared before the break stays cleared"
    );
}
