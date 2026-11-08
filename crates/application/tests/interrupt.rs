//! Interrupting a turn: every call is still answered, in order, and the
//! history stays one the next turn can send.

mod common;

use std::sync::Arc;

use agent_application::agent::{
    AgentDependencies, AgentLoop, AgentLoopConfig, DefaultPromptBuilder, Interrupt, Session,
};
use agent_application::tools::ToolRegistry;
use agent_application::tools::file::WriteFileTool;
use agent_domain::error::{ApprovalError, LlmError, ToolError};
use agent_domain::model::conversation::Conversation;
use agent_domain::model::llm::{
    ChatRequest, ChatResponse, ProviderCapabilities, ProviderId, StopReason,
};
use agent_domain::model::message::{ContentBlock, Message, Role};
use agent_domain::model::tool::{ToolCall, ToolDefinition, ToolName, ToolOutcome, ToolSafety};
use agent_domain::ports::approval::{ApprovalDecision, ApprovalGate, ApprovalRequest};
use agent_domain::ports::events::{AgentEvent, FinishReason, NullEventSink};
use agent_domain::ports::llm::{ChatStream, LlmProvider};
use agent_domain::ports::tool::Tool;
use async_trait::async_trait;
use futures::StreamExt as _;
use serde_json::json;

use common::*;

/// What it is told about a call that was cut off while running.
const CUT_OFF: &str = "may have been partially applied";

/// A tool that presses Ctrl-C on its own call and then never finishes - the
/// long-running command the user gave up on.
struct InterruptingTool {
    interrupt: Interrupt,
}

#[async_trait]
impl Tool for InterruptingTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: ToolName::new("slow_command").unwrap(),
            description: "takes forever".into(),
            input_schema: json!({"type": "object"}),
            safety: ToolSafety::Mutating,
        }
    }

    async fn preview(
        &self,
        _arguments: &serde_json::Value,
    ) -> Result<Vec<agent_domain::model::tool::PlannedWrite>, ToolError> {
        Ok(Vec::new())
    }

    async fn execute(&self, _arguments: serde_json::Value) -> Result<ToolOutcome, ToolError> {
        self.interrupt.fire();
        std::future::pending().await
    }
}

/// A gate that is interrupted while its prompt is on screen.
struct InterruptedPrompt {
    interrupt: Interrupt,
}

#[async_trait]
impl ApprovalGate for InterruptedPrompt {
    async fn authorize(&self, _: &ApprovalRequest) -> Result<ApprovalDecision, ApprovalError> {
        self.interrupt.fire();
        std::future::pending().await
    }
}

/// A model that is interrupted while the user waits for its answer.
struct InterruptedProvider {
    interrupt: Interrupt,
    stream: bool,
}

#[async_trait]
impl LlmProvider for InterruptedProvider {
    fn id(&self) -> ProviderId {
        ProviderId::new("interrupted")
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            supports_streaming: self.stream,
            ..ProviderCapabilities::default()
        }
    }

    async fn chat(&self, _request: ChatRequest) -> Result<ChatResponse, LlmError> {
        assert!(
            !self.stream,
            "a streaming test must not take the plain call"
        );
        self.interrupt.fire();
        std::future::pending().await
    }

    async fn chat_stream(&self, _request: ChatRequest) -> Result<ChatStream, LlmError> {
        // Half a sentence arrives, then the user presses Ctrl-C, then nothing.
        let interrupt = self.interrupt.clone();
        let mut fired = false;
        let rest = futures::stream::poll_fn(move |_| {
            if !fired {
                fired = true;
                interrupt.fire();
            }
            std::task::Poll::Pending
        });
        Ok(Box::pin(
            futures::stream::iter(vec![delta("I will start by")]).chain(rest),
        ))
    }
}

fn multi_tool_use(calls: &[ToolCall]) -> ChatResponse {
    response(
        Message::assistant(calls.iter().cloned().map(ContentBlock::ToolCall).collect()),
        StopReason::ToolUse,
    )
}

/// Write, then the command the user interrupts, then a write that never
/// gets to run.
fn interrupted_batch() -> Vec<ToolCall> {
    calls(&[
        (
            "w1",
            "write_file",
            json!({"path": "first.txt", "content": "1"}),
        ),
        ("slow", "slow_command", json!({})),
        (
            "w2",
            "write_file",
            json!({"path": "second.txt", "content": "2"}),
        ),
    ])
}

fn interruptible_registry(
    file_system: Arc<MemoryFileSystem>,
    interrupt: &Interrupt,
) -> Arc<ToolRegistry> {
    Arc::new(
        ToolRegistry::new()
            .with(Arc::new(WriteFileTool::new(file_system, root())))
            .with(Arc::new(InterruptingTool {
                interrupt: interrupt.clone(),
            })),
    )
}

/// Ids of the calls in `messages` that no result answers - invariant §4,
/// from the other side than [`orphaned_results`].
fn unanswered_calls(messages: &[Message]) -> Vec<String> {
    let answered: std::collections::HashSet<&str> = messages
        .iter()
        .flat_map(|message| message.content.iter())
        .filter_map(|block| match block {
            ContentBlock::ToolResult(result) => Some(result.call_id.as_str()),
            _ => None,
        })
        .collect();
    messages
        .iter()
        .flat_map(Message::tool_calls)
        .map(|call| call.id.as_str())
        .filter(|id| !answered.contains(id))
        .map(str::to_string)
        .collect()
}

fn mentions_interrupted_marker(messages: &[Message]) -> bool {
    messages.iter().any(|message| {
        message.content.iter().any(|block| match block {
            ContentBlock::Text { text } => text.contains("[interrupted]"),
            ContentBlock::ToolResult(result) => result.content.contains("[interrupted]"),
            ContentBlock::ToolCall(_) | ContentBlock::Image(_) | ContentBlock::Opaque { .. } => {
                false
            }
        })
    })
}

#[tokio::test]
async fn an_interrupted_batch_still_answers_every_call_in_order() {
    let file_system = MemoryFileSystem::with(&[]);
    let interrupt = Interrupt::new();
    let events = Arc::new(RecordingSink::default());
    let batch = interrupted_batch();

    let results = dispatcher(
        interruptible_registry(file_system.clone(), &interrupt),
        Arc::new(AlwaysApprove),
        events.clone(),
    )
    .dispatch(&batch, &interrupt)
    .await;

    assert_eq!(
        results.len(),
        batch.len(),
        "one result per call: {results:?}"
    );
    let ids: Vec<&str> = results.iter().map(|r| r.call_id.as_str()).collect();
    assert_eq!(ids, ["w1", "slow", "w2"], "in the order the model asked");

    assert!(
        !results[0].is_error,
        "a finished call keeps its real result"
    );
    assert_eq!(file_system.get("first.txt").as_deref(), Some("1"));

    assert!(results[1].is_error);
    assert!(
        results[1].content.contains(CUT_OFF),
        "the call that was running may have left something behind: {}",
        results[1].content
    );

    assert!(results[2].is_error);
    assert!(
        results[2].content.contains(NOT_STARTED),
        "the call that never ran says so: {}",
        results[2].content
    );
    assert_eq!(
        file_system.get("second.txt"),
        None,
        "and indeed did not run"
    );

    let events = events.events.lock().unwrap();
    let started: Vec<&str> = events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::ToolCallStarted { call, .. } => Some(call.id.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        started,
        ["w1", "slow"],
        "a call that never ran is never shown starting"
    );
    let finished: Vec<(&str, bool)> = events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::ToolCallFinished {
                call_id, is_error, ..
            } => Some((call_id.as_str(), *is_error)),
            _ => None,
        })
        .collect();
    assert_eq!(
        finished,
        [("w1", false), ("slow", true)],
        "the cut-off call is closed on screen as a failure"
    );
}

#[tokio::test]
async fn an_interrupt_at_the_approval_prompt_runs_nothing() {
    let file_system = MemoryFileSystem::with(&[]);
    let interrupt = Interrupt::new();
    let events = Arc::new(RecordingSink::default());
    let batch = interrupted_batch();

    let results = dispatcher(
        interruptible_registry(file_system.clone(), &interrupt),
        Arc::new(InterruptedPrompt {
            interrupt: interrupt.clone(),
        }),
        events.clone(),
    )
    .dispatch(&batch, &interrupt)
    .await;

    assert_eq!(results.len(), batch.len());
    for result in &results {
        assert!(
            result.is_error && result.content.contains(NOT_STARTED),
            "{result:?}"
        );
    }
    assert_eq!(file_system.get("first.txt"), None);
    let events = events.events.lock().unwrap();
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, AgentEvent::ToolCallStarted { .. })),
        "{events:?}"
    );
}

#[tokio::test]
async fn interrupting_the_model_request_returns_interrupted_and_adds_nothing() {
    for stream in [false, true] {
        let interrupt = Interrupt::new();
        let provider = Arc::new(InterruptedProvider {
            interrupt: interrupt.clone(),
            stream,
        });
        let log = Arc::new(RecordingLog::default());
        let agent = AgentLoop::new(
            AgentDependencies {
                llm: provider,
                tools: registry(MemoryFileSystem::with(&[])),
                approval: Arc::new(AlwaysApprove),
                events: Arc::new(NullEventSink),
                context: Arc::new(FakeContext),
                prompt: Arc::new(DefaultPromptBuilder),
                log: log.clone(),
            },
            AgentLoopConfig {
                stream,
                ..AgentLoopConfig::default()
            },
        );

        let mut session = Session::new("int-model");
        let outcome = agent
            .run_until(&mut session, "refactor everything", &interrupt)
            .await
            .expect("an interrupt is not a failure");

        assert_eq!(outcome.reason, FinishReason::Interrupted, "stream={stream}");
        assert!(!outcome.is_complete());
        let messages = session.conversation.messages();
        assert_eq!(
            messages.len(),
            1,
            "stream={stream}: only the user's message - no half answer: {messages:?}"
        );
        assert_eq!(messages[0].role, Role::User);
        let logged = log.seen.lock().unwrap().len();
        assert_eq!(
            logged, 1,
            "stream={stream}: the log matches the conversation"
        );
    }
}

#[tokio::test]
async fn an_interrupt_during_tools_leaves_a_history_the_next_turn_can_send() {
    let file_system = MemoryFileSystem::with(&[]);
    let interrupt = Interrupt::new();
    let batch = interrupted_batch();
    let provider = ScriptedProvider::new(vec![
        multi_tool_use(&batch),
        assistant("Understood, I will leave second.txt alone."),
    ]);
    let log = Arc::new(RecordingLog::default());
    let agent = loop_with_log(
        provider.clone(),
        interruptible_registry(file_system.clone(), &interrupt),
        log.clone(),
    );

    let mut session = Session::new("int-tools");
    let outcome = agent
        .run_until(&mut session, "write both files", &interrupt)
        .await
        .expect("an interrupt is not a failure");

    assert_eq!(outcome.reason, FinishReason::Interrupted);
    assert_eq!(
        provider.requests().len(),
        1,
        "no request after the interrupt"
    );

    let messages = session.conversation.messages().to_vec();
    let roles: Vec<Role> = messages.iter().map(|m| m.role).collect();
    assert_eq!(roles, [Role::User, Role::Assistant, Role::Tool]);
    assert!(unanswered_calls(&messages).is_empty(), "{messages:?}");
    assert!(orphaned_results(&messages).is_empty(), "{messages:?}");
    assert!(
        !mentions_interrupted_marker(&messages),
        "nothing is written into the history beyond the results"
    );

    // The record says exactly what memory says.
    let logged: Vec<Message> = log
        .seen
        .lock()
        .unwrap()
        .iter()
        .map(|(_, m)| m.clone())
        .collect();
    assert_eq!(logged, messages, "the log and the conversation agree");

    // Resuming from that record loses nothing and orphans nothing: there is
    // no trailing unanswered call for the replay to drop.
    let mut replayed = Conversation::from_messages(logged);
    assert_eq!(replayed.drop_trailing_unanswered_calls(), 0);
    let resumed = Session::resumed("int-tools", replayed);
    assert_eq!(resumed.conversation.messages(), &messages[..]);
    assert!(unanswered_calls(resumed.conversation.messages()).is_empty());

    // And the next turn carries on, with the model told what happened.
    let outcome = agent
        .run(&mut session, "fine, stop there")
        .await
        .expect("the session carries on");
    assert_eq!(outcome.reason, FinishReason::Completed);
    let next = &provider.requests()[1];
    assert!(orphaned_results(&next.messages).is_empty());
    assert!(unanswered_calls(&next.messages).is_empty());
    let sent = format!("{:?}", next.messages);
    assert!(sent.contains(CUT_OFF), "{sent}");
    assert!(sent.contains(NOT_STARTED), "{sent}");
}

#[tokio::test]
async fn an_interrupt_before_the_first_request_sends_nothing() {
    let provider = ScriptedProvider::new(vec![assistant("unused")]);
    let agent = loop_with(
        provider.clone(),
        registry(MemoryFileSystem::with(&[])),
        Arc::new(AlwaysApprove),
        Arc::new(NullEventSink),
        10,
    );
    let interrupt = Interrupt::new();
    interrupt.fire();

    let mut session = Session::new("int-early");
    let outcome = agent
        .run_until(&mut session, "hello", &interrupt)
        .await
        .unwrap();

    assert_eq!(outcome.reason, FinishReason::Interrupted);
    assert!(provider.requests().is_empty());
    assert_eq!(session.conversation.len(), 1, "the user's message is kept");
}
