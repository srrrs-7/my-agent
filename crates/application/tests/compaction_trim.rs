//! Fitting the history into its budget: compaction folds the front into a
//! summary, trimming guarantees the budget, and the plan note takes its share.

mod common;

use std::sync::Arc;

use agent_application::agent::{AgentLoopConfig, Session};
use agent_domain::error::LlmError;
use agent_domain::model::conversation::Conversation;
use agent_domain::model::llm::{
    ChatRequest, ChatResponse, ProviderCapabilities, ProviderId, TaskKind, TokenUsage,
};
use agent_domain::model::message::{Message, Role};
use agent_domain::ports::events::{AgentEvent, FinishReason, NullEventSink};
use agent_domain::ports::llm::LlmProvider;
use async_trait::async_trait;
use serde_json::json;

use common::*;

/// Answers ordinary turns from a script but refuses to summarise.
///
/// The fallback exists for exactly this: the compaction request is an extra
/// call that can fail on its own (a rate limit, a model with no spare context),
/// and a failure there must not take the turn down with it.
struct RefusesToSummarize(Arc<ScriptedProvider>);

#[async_trait]
impl LlmProvider for RefusesToSummarize {
    fn id(&self) -> ProviderId {
        self.0.id()
    }

    fn capabilities(&self) -> ProviderCapabilities {
        self.0.capabilities()
    }

    async fn chat(&self, request: ChatRequest) -> Result<ChatResponse, LlmError> {
        if request.metadata.task_kind == TaskKind::Summarize {
            return Err(LlmError::InvalidResponse("no summary for you".into()));
        }
        self.0.chat(request).await
    }
}

#[tokio::test]
async fn an_overflowing_history_is_folded_into_a_summary_instead_of_being_dropped() {
    let provider = ScriptedProvider::new(vec![assistant("RECORD OF THE SESSION"), assistant("ok")]);
    let events = Arc::new(RecordingSink::default());
    let agent = loop_with_config(
        provider.clone(),
        registry(MemoryFileSystem::default().into()),
        Arc::new(AlwaysApprove),
        events.clone(),
        compacting_config(8 * 1024),
    );

    let mut session = crowded_session("c1");
    agent.run(&mut session, "carry on").await.unwrap();

    let requests = provider.requests();
    assert_eq!(
        requests[0].metadata.task_kind,
        TaskKind::Summarize,
        "the first call must be the summary, before the turn itself"
    );
    assert!(
        requests[0].tools.is_empty(),
        "a summary request advertises no tools"
    );

    let turn = &requests[1];
    assert_eq!(
        turn.metadata.task_kind,
        TaskKind::Agentic,
        "the turn itself is not a summary"
    );
    assert!(
        turn.messages[0].text().contains("RECORD OF THE SESSION"),
        "the summary must be what the model now sees first: {:?}",
        turn.messages[0].text()
    );

    let compacted = events
        .events
        .lock()
        .unwrap()
        .iter()
        .any(|event| matches!(event, AgentEvent::HistoryCompacted { .. }));
    assert!(compacted, "the user must be told the history was compacted");
}

#[tokio::test]
async fn the_summary_request_is_counted_in_the_session_usage() {
    // The summary is a request on the same bill as the turn. Dropping its
    // usage made long sessions - the only ones that compact - look cheaper
    // than they were, which is precisely what an eval comparing token counts
    // must not be told.
    let summary = ChatResponse {
        usage: TokenUsage {
            input_tokens: 1_000,
            output_tokens: 200,
            ..TokenUsage::default()
        },
        ..assistant("RECORD OF THE SESSION")
    };
    let provider = ScriptedProvider::new(vec![summary, assistant("ok")]);
    let agent = loop_with_config(
        provider.clone(),
        registry(MemoryFileSystem::default().into()),
        Arc::new(AlwaysApprove),
        Arc::new(NullEventSink),
        compacting_config(8 * 1024),
    );

    let mut session = crowded_session("c-usage");
    let outcome = agent.run(&mut session, "carry on").await.unwrap();

    assert_eq!(
        provider.requests()[0].metadata.task_kind,
        TaskKind::Summarize,
        "precondition: the history really was compacted"
    );
    // 1000/200 for the summary, 10/5 for the turn.
    assert_eq!(session.usage.input_tokens, 1_010, "{:?}", session.usage);
    assert_eq!(session.usage.output_tokens, 205, "{:?}", session.usage);
    assert_eq!(
        outcome.usage, session.usage,
        "the outcome reports the same total"
    );
}

#[tokio::test]
async fn a_compacted_history_is_still_well_formed() {
    // `compact_keep_recent: 3` leaves a complete tool turn in the tail, so the
    // pairing check below has something to be wrong about. With a smaller tail
    // the whole history folds away and the check would pass on an empty set.
    let provider = ScriptedProvider::new(vec![assistant("RECORD"), assistant("ok")]);
    let agent = loop_with_config(
        provider.clone(),
        registry(MemoryFileSystem::default().into()),
        Arc::new(AlwaysApprove),
        Arc::new(NullEventSink),
        AgentLoopConfig {
            // 4 puts the requested cut squarely on a tool result *and* leaves a
            // complete tool turn in the tail, so this exercises both boundary
            // rules and still has a pair left to check.
            compact_keep_recent: 4,
            ..compacting_config(8 * 1024)
        },
    );

    let mut session = crowded_session("c2");
    agent.run(&mut session, "carry on").await.unwrap();

    let sent = &provider.requests()[1].messages;
    assert!(
        sent[0].text().contains("RECORD"),
        "the compaction must actually have run, or the rest of this test is \
         checking an untouched history: {:?}",
        sent[0].text()
    );
    assert_eq!(
        sent[0].role,
        Role::User,
        "the history must open with a user message"
    );
    assert!(
        sent.iter().any(Message::has_tool_calls),
        "the tail must still hold a tool turn, or this test proves nothing"
    );
    assert!(
        orphaned_results(sent).is_empty(),
        "tool results left without their call: {:?}",
        orphaned_results(sent)
    );
}

#[tokio::test]
async fn a_failed_summary_falls_back_to_trimming_and_the_turn_still_finishes() {
    let scripted = ScriptedProvider::new(vec![assistant("answered anyway")]);
    let provider = Arc::new(RefusesToSummarize(scripted));
    let events = Arc::new(RecordingSink::default());
    let agent = loop_with_config(
        provider,
        registry(MemoryFileSystem::default().into()),
        Arc::new(AlwaysApprove),
        events.clone(),
        compacting_config(8 * 1024),
    );

    let mut session = crowded_session("c3");
    let outcome = agent.run(&mut session, "carry on").await.unwrap();

    assert_eq!(outcome.final_text, "answered anyway");
    assert_eq!(outcome.reason, FinishReason::Completed);

    let recorded = events.events.lock().unwrap();
    assert!(
        recorded
            .iter()
            .any(|event| matches!(event, AgentEvent::HistoryTrimmed { .. })),
        "the old path must still run when the summary does not arrive"
    );
    assert!(
        !recorded
            .iter()
            .any(|event| matches!(event, AgentEvent::HistoryCompacted { .. })),
        "a failed compaction must not be reported as one"
    );
}

#[tokio::test]
async fn a_history_inside_its_budget_costs_no_extra_request() {
    let provider = ScriptedProvider::new(vec![assistant("ok")]);
    let agent = loop_with_config(
        provider.clone(),
        registry(MemoryFileSystem::default().into()),
        Arc::new(AlwaysApprove),
        Arc::new(NullEventSink),
        compacting_config(1024 * 1024),
    );

    let mut session = crowded_session("c4");
    agent.run(&mut session, "carry on").await.unwrap();

    assert!(
        provider
            .requests()
            .iter()
            .all(|request| request.metadata.task_kind != TaskKind::Summarize),
        "compaction must not run while the history still fits"
    );
}

#[tokio::test]
async fn compaction_can_be_turned_off() {
    let provider = ScriptedProvider::new(vec![assistant("ok")]);
    let events = Arc::new(RecordingSink::default());
    let agent = loop_with_config(
        provider.clone(),
        registry(MemoryFileSystem::default().into()),
        Arc::new(AlwaysApprove),
        events.clone(),
        AgentLoopConfig {
            compact: false,
            ..compacting_config(8 * 1024)
        },
    );

    let mut session = crowded_session("c5");
    agent.run(&mut session, "carry on").await.unwrap();

    assert!(
        provider
            .requests()
            .iter()
            .all(|request| request.metadata.task_kind != TaskKind::Summarize),
        "AGENT_COMPACT=false must mean no extra request at all"
    );
    assert!(
        events
            .events
            .lock()
            .unwrap()
            .iter()
            .any(|event| matches!(event, AgentEvent::HistoryTrimmed { .. })),
        "trimming still has to keep the budget"
    );
}

/// Once the history reaches the budget, trimming to exactly the budget would
/// drop a message or two from the front on every iteration, and each such
/// drop rewrites the prompt-cached prefix. One trim goes well below instead,
/// and the next iterations leave the front alone.
#[tokio::test]
async fn a_trim_makes_room_for_several_turns_without_touching_the_front_again() {
    const BUDGET: usize = 16 * 1024;
    let file_system = MemoryFileSystem::with(&[("src/main.rs", "fn main() {}\n")]);
    let provider = ScriptedProvider::new(vec![
        tool_use("r1", "read_file", json!({"path": "src/main.rs"})),
        tool_use("r2", "read_file", json!({"path": "src/main.rs"})),
        tool_use("r3", "read_file", json!({"path": "src/main.rs"})),
        assistant("done"),
    ]);
    let events = Arc::new(RecordingSink::default());
    let agent = loop_with_config(
        provider.clone(),
        registry_with_plan(file_system),
        Arc::new(AlwaysApprove),
        events.clone(),
        AgentLoopConfig {
            compact: false,
            keep_recent_messages: 2,
            max_iterations: 5,
            ..compacting_config(BUDGET)
        },
    );

    let mut session = crowded_session("hysteresis");
    assert!(
        session.conversation.approx_bytes() > BUDGET,
        "precondition: over budget"
    );
    agent.run(&mut session, "carry on").await.unwrap();

    let requests = provider.requests();
    assert_eq!(requests.len(), 4);
    let sent = |index: usize| -> usize {
        requests[index]
            .messages
            .iter()
            .map(Message::approx_bytes)
            .sum()
    };
    assert!(
        sent(0) <= BUDGET * 3 / 4,
        "trimmed to the low-water mark, not just under the budget: {} bytes",
        sent(0)
    );
    for index in 1..requests.len() {
        assert_eq!(
            requests[index].messages[0], requests[0].messages[0],
            "request {index}: the front of the history changed"
        );
        assert!(
            sent(index) <= BUDGET,
            "request {index}: {} bytes",
            sent(index)
        );
    }
    let trims = events
        .events
        .lock()
        .unwrap()
        .iter()
        .filter(|event| matches!(event, AgentEvent::HistoryTrimmed { .. }))
        .count();
    assert_eq!(trims, 1, "one trim, then room to spare");
}

/// The plan note is reserved for in the budget - but a copy already in the
/// history is counted there, and reserving for it as well would trim a
/// history that fits.
#[tokio::test]
async fn a_note_already_in_the_history_is_not_reserved_for_twice() {
    // Obtain the exact note the loop writes for this plan.
    let note = {
        let provider = ScriptedProvider::new(vec![assistant("ok")]);
        let agent = loop_with_config(
            provider.clone(),
            registry_with_plan(MemoryFileSystem::default().into()),
            Arc::new(AlwaysApprove),
            Arc::new(NullEventSink),
            AgentLoopConfig {
                compact: false,
                keep_recent_messages: 2,
                ..compacting_config(8 * 1024)
            },
        );
        let mut session = Session::resumed("note-source", planned_record(UNFINISHED));
        agent.run(&mut session, "carry on").await.unwrap();
        let note = provider.requests()[0].messages.last().unwrap().clone();
        assert!(
            note.text().starts_with(NOTE),
            "precondition: {}",
            note.text()
        );
        note
    };

    // A history that wrote the plan, carries the note, and fits the budget -
    // but not the budget minus another note.
    let planned = planned_record(UNFINISHED).into_messages();
    let mut messages = planned[..3].to_vec();
    messages.push(Message::user(note.text()));
    let history = Conversation::from_messages(messages);
    let input = Message::user("carry on");
    let total = history.approx_bytes() + input.approx_bytes();
    let budget = total + note.approx_bytes() / 2;

    let provider = ScriptedProvider::new(vec![assistant("ok")]);
    let events = Arc::new(RecordingSink::default());
    let agent = loop_with_config(
        provider.clone(),
        registry_with_plan(MemoryFileSystem::default().into()),
        Arc::new(AlwaysApprove),
        events.clone(),
        AgentLoopConfig {
            compact: false,
            keep_recent_messages: 1,
            ..compacting_config(budget)
        },
    );
    let mut session = Session::resumed("noted", history);
    agent.run(&mut session, "carry on").await.unwrap();

    assert!(
        !events
            .events
            .lock()
            .unwrap()
            .iter()
            .any(|event| matches!(event, AgentEvent::HistoryTrimmed { .. })),
        "the history fit; nothing should have been dropped"
    );
    assert_eq!(provider.requests()[0].messages.len(), 5);
}
