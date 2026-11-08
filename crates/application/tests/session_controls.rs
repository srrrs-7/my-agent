//! What `/model`, `/compact` and `/context` do to a session between turns.

mod common;

use std::sync::Arc;

use agent_application::agent::{AgentLoop, AgentLoopConfig, Interrupt, ManualCompaction, Session};
use agent_domain::model::llm::{ModelId, StopReason, TaskKind, TokenUsage};
use agent_domain::model::message::{ContentBlock, Message};
use agent_domain::model::tool::{ToolCall, ToolCallId, ToolName};
use agent_domain::ports::events::NullEventSink;
use serde_json::json;

use common::*;

fn agent_with(provider: Arc<ScriptedProvider>, config: AgentLoopConfig) -> AgentLoop {
    loop_with_config(
        provider,
        registry_with_plan(MemoryFileSystem::with(&[])),
        Arc::new(AlwaysApprove),
        Arc::new(NullEventSink),
        config,
    )
}

fn thinking() -> ContentBlock {
    ContentBlock::Opaque {
        dialect: "anthropic".into(),
        data: json!({"type": "thinking", "thinking": "", "signature": "sig"}),
    }
}

fn has_opaque(messages: &[Message]) -> bool {
    messages
        .iter()
        .flat_map(|message| &message.content)
        .any(|block| matches!(block, ContentBlock::Opaque { .. }))
}

fn costing(input_tokens: u64, cache_read_tokens: u64) -> TokenUsage {
    TokenUsage {
        input_tokens,
        output_tokens: 7,
        cache_read_tokens,
        cache_write_tokens: 3,
    }
}

#[tokio::test]
async fn a_switched_model_is_what_the_next_request_asks_for() {
    let provider = ScriptedProvider::new(vec![assistant("one"), assistant("two")]);
    let agent = agent_with(
        provider.clone(),
        AgentLoopConfig {
            model: Some(ModelId::new("configured")),
            ..AgentLoopConfig::default()
        },
    );
    let mut session = Session::new("m1");

    agent.run(&mut session, "first").await.unwrap();
    session.switch_model(ModelId::new("cloud/claude-sonnet-5-5"));
    agent.run(&mut session, "second").await.unwrap();

    let models: Vec<_> = provider
        .requests()
        .iter()
        .map(|request| request.model.clone().map(|m| m.as_str().to_string()))
        .collect();
    assert_eq!(
        models,
        [
            Some("configured".to_string()),
            Some("cloud/claude-sonnet-5-5".to_string())
        ]
    );
    assert_eq!(
        agent.model(&session),
        Some(ModelId::new("cloud/claude-sonnet-5-5"))
    );
    assert_eq!(
        agent.model(&Session::new("fresh")),
        Some(ModelId::new("configured")),
        "the override belongs to the session, not to the loop"
    );
}

#[tokio::test]
async fn switching_models_leaves_the_previous_models_thinking_behind_and_every_call_answered() {
    let call = ToolCall::new(
        ToolCallId::new("c1"),
        ToolName::new("read_file").unwrap(),
        json!({"path": "a.rs"}),
    );
    let provider = ScriptedProvider::new(vec![
        response(
            Message::assistant(vec![thinking(), ContentBlock::ToolCall(call.clone())]),
            StopReason::ToolUse,
        ),
        response(
            Message::assistant(vec![thinking(), ContentBlock::text("read it")]),
            StopReason::EndTurn,
        ),
        assistant("on the new model"),
    ]);
    let agent = agent_with(provider.clone(), AgentLoopConfig::default());
    let mut session = Session::new("m2");

    agent.run(&mut session, "read a.rs").await.unwrap();
    assert!(
        has_opaque(session.conversation.messages()),
        "precondition: the first model left thinking blocks"
    );
    let dropped = session.switch_model(ModelId::new("other"));
    agent.run(&mut session, "and now?").await.unwrap();

    assert_eq!(dropped, 2);
    let last = provider.requests().pop().unwrap();
    assert!(!has_opaque(&last.messages), "{:?}", last.messages);
    assert!(
        orphaned_results(&last.messages).is_empty(),
        "{:?}",
        last.messages
    );
    assert!(
        last.messages
            .iter()
            .any(|m| m.tool_calls().any(|c| c.id == call.id)),
        "the call itself stays"
    );
    assert!(
        last.messages
            .iter()
            .flat_map(|m| &m.content)
            .any(|b| matches!(b, ContentBlock::ToolResult(r) if r.call_id == call.id)),
        "and so does its result"
    );
}

#[tokio::test]
async fn a_manual_compaction_runs_even_with_compaction_switched_off() {
    let provider = ScriptedProvider::new(vec![assistant("SUMMARY OF EARLIER WORK")]);
    let agent = agent_with(
        provider.clone(),
        AgentLoopConfig {
            compact: false,
            compact_keep_recent: 2,
            // Far above the history: the loop itself would see no reason to
            // shorten anything.
            max_history_bytes: 1024 * 1024,
            ..AgentLoopConfig::default()
        },
    );
    let mut session = crowded_session("c1");
    let before = session.conversation.approx_bytes();

    let outcome = agent.compact_now(&mut session, &Interrupt::new()).await;

    let ManualCompaction::Compacted(report) = outcome else {
        panic!("expected a compaction, got {outcome:?}");
    };
    assert_eq!(report.before_bytes, before);
    assert_eq!(report.after_bytes, session.conversation.approx_bytes());
    assert!(report.after_bytes < report.before_bytes, "{report:?}");
    let requests = provider.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].metadata.task_kind, TaskKind::Summarize);
    assert!(
        session.conversation.messages()[0]
            .text()
            .contains("SUMMARY OF EARLIER WORK")
    );
    assert_eq!(session.compactions(), 1);
}

#[tokio::test]
async fn the_summary_requests_usage_is_added_to_the_sessions() {
    let mut summary = assistant("SUMMARY");
    summary.usage = costing(400, 0);
    let provider = ScriptedProvider::new(vec![summary]);
    let agent = agent_with(
        provider,
        AgentLoopConfig {
            compact_keep_recent: 2,
            ..AgentLoopConfig::default()
        },
    );
    let mut session = crowded_session("c2");
    session.usage = costing(1000, 0);

    let outcome = agent.compact_now(&mut session, &Interrupt::new()).await;

    assert!(
        matches!(outcome, ManualCompaction::Compacted(report) if report.usage == costing(400, 0))
    );
    assert_eq!(session.usage.input_tokens, 1400);
    assert_eq!(session.usage.output_tokens, 14);
    assert_eq!(
        session.last_usage(),
        None,
        "the summary is not a request of the conversation"
    );
}

#[tokio::test]
async fn a_manual_compaction_asks_the_model_the_session_runs_on() {
    let provider = ScriptedProvider::new(vec![assistant("SUMMARY")]);
    let agent = agent_with(
        provider.clone(),
        AgentLoopConfig {
            model: Some(ModelId::new("configured")),
            compact_keep_recent: 2,
            ..AgentLoopConfig::default()
        },
    );
    let mut session = crowded_session("c3");
    session.switch_model(ModelId::new("cloud/claude-sonnet-5-5"));

    agent.compact_now(&mut session, &Interrupt::new()).await;

    assert_eq!(
        provider.requests()[0].model,
        Some(ModelId::new("cloud/claude-sonnet-5-5"))
    );
}

#[tokio::test]
async fn a_manual_compaction_keeps_the_plan_in_front_of_the_model() {
    let provider = ScriptedProvider::new(vec![assistant("SUMMARY")]);
    let agent = agent_with(
        provider,
        AgentLoopConfig {
            compact: false,
            compact_keep_recent: 2,
            ..AgentLoopConfig::default()
        },
    );
    let mut session = Session::resumed("c4", planned_record(UNFINISHED));

    let outcome = agent.compact_now(&mut session, &Interrupt::new()).await;

    assert!(
        matches!(outcome, ManualCompaction::Compacted(_)),
        "{outcome:?}"
    );
    let messages = session.conversation.messages();
    assert!(
        !messages
            .iter()
            .flat_map(Message::tool_calls)
            .any(|c| c.name.as_str() == "todo_write"),
        "precondition: the call that wrote the plan was folded away"
    );
    let note = messages.last().unwrap().text();
    assert!(note.starts_with(NOTE), "the note comes last: {note}");
    assert!(note.contains("[>] write the lexer"), "{note}");
}

#[tokio::test]
async fn a_history_too_short_to_fold_makes_no_request() {
    let provider = ScriptedProvider::new(vec![assistant("SUMMARY")]);
    let agent = agent_with(provider.clone(), AgentLoopConfig::default());
    let mut session = Session::new("c5");
    session.conversation.push(Message::user("hi"));

    let outcome = agent.compact_now(&mut session, &Interrupt::new()).await;

    assert_eq!(outcome, ManualCompaction::NothingToFold);
    assert!(provider.requests().is_empty());
}

#[tokio::test]
async fn an_interrupted_manual_compaction_changes_nothing() {
    let provider = ScriptedProvider::new(vec![assistant("SUMMARY")]);
    let agent = agent_with(
        provider,
        AgentLoopConfig {
            compact_keep_recent: 2,
            ..AgentLoopConfig::default()
        },
    );
    let mut session = crowded_session("c6");
    let before = session.conversation.clone();
    let interrupt = Interrupt::new();
    interrupt.fire();

    let outcome = agent.compact_now(&mut session, &interrupt).await;

    assert_eq!(outcome, ManualCompaction::Interrupted);
    assert_eq!(session.conversation, before);
    assert_eq!(session.compactions(), 0);
}

#[tokio::test]
async fn the_last_usage_is_the_newest_responses_not_a_total() {
    let mut first = assistant("one");
    first.usage = costing(100, 0);
    let mut second = assistant("two");
    second.usage = costing(120, 90);
    let provider = ScriptedProvider::new(vec![first, second]);
    let agent = agent_with(provider, AgentLoopConfig::default());
    let mut session = Session::new("u1");
    assert_eq!(session.last_usage(), None);

    agent.run(&mut session, "first").await.unwrap();
    assert_eq!(session.last_usage(), Some(costing(100, 0)));
    agent.run(&mut session, "second").await.unwrap();

    assert_eq!(session.last_usage(), Some(costing(120, 90)));
    assert_eq!(session.usage.input_tokens, 220, "the total still adds up");
}

#[tokio::test]
async fn the_context_report_accounts_for_each_part_of_the_request() {
    let provider = ScriptedProvider::new(vec![assistant("done")]);
    let agent = agent_with(
        provider.clone(),
        AgentLoopConfig {
            max_history_bytes: 200_000,
            ..AgentLoopConfig::default()
        },
    );
    let mut session = Session::new("r1");

    let before = agent.context_report(&session);
    assert_eq!(
        before.system_bytes, None,
        "not rendered before the first turn"
    );
    assert_eq!(
        session.system_prompt(),
        None,
        "and reporting does not render it"
    );
    assert_eq!(before.messages, 0);
    assert_eq!(before.last_usage, None);

    agent.run(&mut session, "hello").await.unwrap();
    let request = provider.requests().pop().unwrap();
    let report = agent.context_report(&session);

    assert_eq!(
        report.system_bytes,
        request.system.as_ref().map(String::len)
    );
    assert_eq!(report.tool_count, request.tools.len());
    assert_eq!(report.tool_count, 3);
    let tools_bytes: usize = request
        .tools
        .iter()
        .map(|t| t.name.as_str().len() + t.description.len() + t.input_schema.to_string().len())
        .sum();
    assert_eq!(report.tools_bytes, tools_bytes);
    assert_eq!(report.messages, 2);
    assert_eq!(report.history_bytes, session.conversation.approx_bytes());
    assert_eq!(report.history_budget, 200_000);
    assert_eq!(report.plan_note, None, "no plan, nothing held back");
    assert_eq!(report.last_usage, session.last_usage());
    assert_eq!(report.compactions, 0);
}

#[tokio::test]
async fn the_context_report_counts_the_plan_note_where_it_is() {
    let provider = ScriptedProvider::new(vec![assistant("SUMMARY")]);
    let agent = agent_with(
        provider,
        AgentLoopConfig {
            compact_keep_recent: 2,
            ..AgentLoopConfig::default()
        },
    );
    let mut session = Session::resumed("r2", planned_record(UNFINISHED));

    let reserved = agent
        .context_report(&session)
        .plan_note
        .expect("a plan note");
    assert!(!reserved.in_history && reserved.bytes > 0, "{reserved:?}");

    agent.compact_now(&mut session, &Interrupt::new()).await;
    let noted = agent
        .context_report(&session)
        .plan_note
        .expect("a plan note");

    assert!(noted.in_history, "{noted:?}");
    assert_eq!(noted.bytes, reserved.bytes);
    assert_eq!(
        noted.bytes,
        session
            .conversation
            .messages()
            .last()
            .unwrap()
            .approx_bytes()
    );
    assert_eq!(agent.context_report(&session).compactions, 1);
}

#[test]
fn a_resumed_session_starts_on_the_configured_model() {
    let mut session = Session::new("r3");
    session.conversation.push(Message::user("hi"));
    session.switch_model(ModelId::new("other"));

    let resumed = Session::resumed("r3", session.conversation.clone());

    assert_eq!(resumed.model_override(), None);
}
