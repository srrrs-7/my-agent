//! `spawn_agent`: a sub-agent investigates in its own history and only its
//! answer comes back.
//!
//! The parent and the sub-agent share one scripted provider, as they share
//! one provider in production, so each script below is the parent's and the
//! sub-agent's requests in the order they are made.

mod common;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use agent_application::agent::{
    AgentDependencies, AgentLoop, AgentLoopConfig, Interrupt, Session, SubagentPromptBuilder,
};
use agent_application::tools::ToolRegistry;
use agent_application::tools::agent::{SpawnAgentTool, SubagentLimits};
use agent_domain::error::{ApprovalError, LlmError};
use agent_domain::model::llm::{
    ChatRequest, ChatResponse, ProviderCapabilities, ProviderId, StopReason, TokenUsage,
};
use agent_domain::model::message::{ContentBlock, Message};
use agent_domain::model::tool::{ToolResult, ToolSafety};
use agent_domain::ports::approval::{ApprovalDecision, ApprovalGate, ApprovalRequest};
use agent_domain::ports::conversation_log::ConversationLog;
use agent_domain::ports::events::{AgentEvent, EventSink, FinishReason};
use agent_domain::ports::llm::LlmProvider;
use async_trait::async_trait;
use serde_json::json;

use common::*;

/// What the read tool returns for the file the sub-agent is sent to read:
/// distinctive, so its absence from the parent can be checked.
const FILE_BODY: &str = "THE-WHOLE-FILE-BODY";

const TASK: &str = "What does src/lib.rs define?";

/// The parent and the sub-agent wired the way the composition root wires
/// them: the sub-agent over the read-only part of `tools`, sharing the
/// gate, the sink and the log; `spawn_agent` added to the parent's tools
/// last.
struct Agents {
    parent: AgentLoop,
}

struct Wiring {
    provider: Arc<dyn LlmProvider>,
    approval: Arc<dyn ApprovalGate>,
    events: Arc<dyn EventSink>,
    log: Arc<dyn ConversationLog>,
    child_iterations: u32,
}

impl Wiring {
    fn new(provider: Arc<dyn LlmProvider>) -> Self {
        Self {
            provider,
            approval: Arc::new(AlwaysApprove),
            events: Arc::new(RecordingSink::default()),
            log: Arc::new(RecordingLog::default()),
            child_iterations: 5,
        }
    }

    fn build(self, tools: ToolRegistry) -> Agents {
        let child = AgentLoop::new(
            AgentDependencies {
                llm: self.provider.clone(),
                tools: Arc::new(tools.read_only()),
                approval: self.approval.clone(),
                events: self.events.clone(),
                context: Arc::new(FakeContext),
                prompt: Arc::new(SubagentPromptBuilder),
                log: self.log.clone(),
            },
            AgentLoopConfig {
                max_iterations: self.child_iterations,
                stream: false,
                ..AgentLoopConfig::default()
            },
        );
        let spawn = SpawnAgentTool::new(
            Arc::new(child),
            SubagentLimits {
                max_answer_bytes: 1024,
            },
        );
        let parent = AgentLoop::new(
            AgentDependencies {
                llm: self.provider,
                tools: Arc::new(tools.with(Arc::new(spawn))),
                approval: self.approval,
                events: self.events,
                context: Arc::new(FakeContext),
                prompt: Arc::new(agent_application::agent::DefaultPromptBuilder),
                log: self.log,
            },
            AgentLoopConfig {
                max_iterations: 5,
                ..AgentLoopConfig::default()
            },
        );
        Agents { parent }
    }
}

/// `read_file` and `write_file` over a workspace holding one file.
fn tools() -> ToolRegistry {
    let file_system = MemoryFileSystem::with(&[("src/lib.rs", FILE_BODY)]);
    ToolRegistry::clone(&registry(file_system))
}

fn spawn(id: &str, task: &str) -> ChatResponse {
    tool_use(id, "spawn_agent", json!({ "task": task }))
}

fn read(id: &str) -> ChatResponse {
    tool_use(id, "read_file", json!({"path": "src/lib.rs"}))
}

/// Parent delegates; the sub-agent reads the file and answers; the parent
/// answers the user.
fn delegation() -> Vec<ChatResponse> {
    vec![
        spawn("s1", TASK),
        read("r1"),
        assistant("It defines the body."),
        assistant("Done: src/lib.rs defines the body."),
    ]
}

/// Every tool result in `session`, in order.
fn results(session: &Session) -> Vec<ToolResult> {
    session
        .conversation
        .messages()
        .iter()
        .flat_map(|message| message.content.iter())
        .filter_map(|block| match block {
            ContentBlock::ToolResult(result) => Some(result.clone()),
            _ => None,
        })
        .collect()
}

fn mentions(messages: &[Message], needle: &str) -> bool {
    messages
        .iter()
        .any(|message| format!("{message:?}").contains(needle))
}

fn tool_names(request: &ChatRequest) -> Vec<String> {
    request
        .tools
        .iter()
        .map(|tool| tool.name.to_string())
        .collect()
}

#[tokio::test]
async fn only_the_sub_agents_answer_reaches_the_parent() {
    let provider = ScriptedProvider::new(delegation());
    let agents = Wiring::new(provider.clone()).build(tools());
    let mut session = Session::new("parent");

    let outcome = agents
        .parent
        .run(&mut session, "Explain src/lib.rs")
        .await
        .unwrap();

    assert!(outcome.is_complete(), "{outcome:?}");
    assert_eq!(outcome.final_text, "Done: src/lib.rs defines the body.");
    let results = results(&session);
    assert_eq!(results.len(), 1, "{results:?}");
    assert_eq!(results[0].content, "It defines the body.");
    assert!(!results[0].is_error);

    // The parent's last request is what its model saw of the delegation.
    let requests = provider.requests();
    assert_eq!(requests.len(), 4);
    assert!(
        !mentions(&requests[3].messages, FILE_BODY),
        "the file the sub-agent read reached the parent's model"
    );
}

#[tokio::test]
async fn the_sub_agents_history_is_its_own_and_never_mixed_into_the_parents() {
    let provider = ScriptedProvider::new(delegation());
    let agents = Wiring::new(provider.clone()).build(tools());
    let mut session = Session::new("parent");

    agents
        .parent
        .run(&mut session, "Explain src/lib.rs")
        .await
        .unwrap();

    // user, the spawn call, its result, the answer - nothing of the child's.
    assert_eq!(session.conversation.len(), 4, "{:?}", session.conversation);
    assert!(!mentions(session.conversation.messages(), FILE_BODY));
    assert!(!mentions(session.conversation.messages(), "\"r1\""));

    // The sub-agent started from nothing but the task, under its own brief.
    let requests = provider.requests();
    let child = &requests[1];
    assert_eq!(child.messages.len(), 1, "{:?}", child.messages);
    assert_eq!(child.messages[0].text(), TASK);
    assert_ne!(child.system, requests[0].system);
    assert!(
        child
            .system
            .as_deref()
            .unwrap_or_default()
            .contains("sub-agent"),
        "{:?}",
        child.system
    );
}

#[tokio::test]
async fn what_the_sub_agent_spent_is_counted_in_the_parents_usage() {
    let provider = ScriptedProvider::new(delegation());
    let agents = Wiring::new(provider).build(tools());
    let mut session = Session::new("parent");

    let outcome = agents
        .parent
        .run(&mut session, "Explain src/lib.rs")
        .await
        .unwrap();

    // Four responses of 10 in / 5 out each: two the parent's, two the child's.
    let expected = TokenUsage {
        input_tokens: 40,
        output_tokens: 20,
        ..TokenUsage::default()
    };
    assert_eq!(outcome.usage, expected);
    assert_eq!(session.usage, expected);
}

#[tokio::test]
async fn the_sub_agent_is_offered_only_the_read_only_tools_and_cannot_spawn() {
    let provider = ScriptedProvider::new(delegation());
    let agents = Wiring::new(provider.clone()).build(tools());

    agents
        .parent
        .run(&mut Session::new("parent"), "Explain src/lib.rs")
        .await
        .unwrap();

    let requests = provider.requests();
    assert_eq!(
        tool_names(&requests[0]),
        ["read_file", "spawn_agent", "write_file"]
    );
    assert_eq!(tool_names(&requests[1]), ["read_file"]);
    assert!(
        requests[0]
            .tools
            .iter()
            .any(|tool| tool.name.as_str() == "spawn_agent" && tool.safety == ToolSafety::ReadOnly),
        "spawn_agent is itself read-only"
    );
}

#[tokio::test]
async fn a_sub_agent_out_of_iterations_is_a_failed_call_and_the_parent_carries_on() {
    let provider = ScriptedProvider::new(vec![
        spawn("s1", TASK),
        // The sub-agent's only iteration goes on a call; its budget is spent.
        read("r1"),
        assistant("I will look myself."),
    ]);
    let mut wiring = Wiring::new(provider);
    wiring.child_iterations = 1;
    let agents = wiring.build(tools());
    let mut session = Session::new("parent");

    let outcome = agents
        .parent
        .run(&mut session, "Explain src/lib.rs")
        .await
        .unwrap();

    assert!(outcome.is_complete(), "{outcome:?}");
    assert_eq!(outcome.final_text, "I will look myself.");
    let result = &results(&session)[0];
    assert!(result.is_error, "{result:?}");
    assert!(
        result.content.contains("1-iteration budget") && result.content.contains("narrower"),
        "the model is told what happened and what to do: {}",
        result.content
    );
}

/// Replays a script in which a request can fail.
struct FlakyProvider {
    script: Mutex<Vec<Result<ChatResponse, LlmError>>>,
}

#[async_trait]
impl LlmProvider for FlakyProvider {
    fn id(&self) -> ProviderId {
        ProviderId::new("flaky")
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities::default()
    }

    async fn chat(&self, _request: ChatRequest) -> Result<ChatResponse, LlmError> {
        let mut script = self.script.lock().unwrap();
        if script.is_empty() {
            return Ok(assistant("(script exhausted)"));
        }
        script.remove(0)
    }
}

#[tokio::test]
async fn a_sub_agent_whose_provider_fails_is_a_failed_call_and_the_parent_carries_on() {
    let provider = Arc::new(FlakyProvider {
        script: Mutex::new(vec![
            Ok(spawn("s1", TASK)),
            Err(LlmError::Api {
                status: 529,
                message: "overloaded".into(),
            }),
            Ok(assistant("Reading it myself, then.")),
        ]),
    });
    let agents = Wiring::new(provider).build(tools());
    let mut session = Session::new("parent");

    let outcome = agents
        .parent
        .run(&mut session, "Explain src/lib.rs")
        .await
        .unwrap();

    assert_eq!(outcome.reason, FinishReason::Completed);
    assert_eq!(outcome.final_text, "Reading it myself, then.");
    let result = &results(&session)[0];
    assert!(result.is_error, "{result:?}");
    assert!(
        result.content.contains("overloaded") && result.content.contains("yourself"),
        "{}",
        result.content
    );
}

/// Serves the parent's spawn call, then presses Ctrl-C while the sub-agent
/// waits for its first answer - which never comes. The wait notes when it
/// is dropped.
struct InterruptedChild {
    interrupt: Interrupt,
    requests: Mutex<usize>,
    child_dropped: Arc<AtomicBool>,
}

struct DropFlag(Arc<AtomicBool>);

impl Drop for DropFlag {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

#[async_trait]
impl LlmProvider for InterruptedChild {
    fn id(&self) -> ProviderId {
        ProviderId::new("interrupted")
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities::default()
    }

    async fn chat(&self, _request: ChatRequest) -> Result<ChatResponse, LlmError> {
        let first = {
            let mut requests = self.requests.lock().unwrap();
            *requests += 1;
            *requests == 1
        };
        if first {
            return Ok(spawn("s1", TASK));
        }
        let _flag = DropFlag(self.child_dropped.clone());
        self.interrupt.fire();
        std::future::pending().await
    }
}

#[tokio::test]
async fn interrupting_the_parent_stops_the_sub_agent() {
    let interrupt = Interrupt::new();
    let child_dropped = Arc::new(AtomicBool::new(false));
    let provider = Arc::new(InterruptedChild {
        interrupt: interrupt.clone(),
        requests: Mutex::new(0),
        child_dropped: child_dropped.clone(),
    });
    let agents = Wiring::new(provider.clone()).build(tools());
    let mut session = Session::new("parent");

    let outcome = agents
        .parent
        .run_until(&mut session, "Explain src/lib.rs", &interrupt)
        .await
        .unwrap();

    assert_eq!(outcome.reason, FinishReason::Interrupted);
    assert!(
        child_dropped.load(Ordering::SeqCst),
        "the sub-agent's request is still pending"
    );
    assert_eq!(*provider.requests.lock().unwrap(), 2);
    let result = &results(&session)[0];
    assert!(result.is_error);
    assert!(
        result.content.contains("Interrupted by the user"),
        "{}",
        result.content
    );
    assert!(orphaned_results(session.conversation.messages()).is_empty());
}

/// Records every call it is asked about and refuses `read_file`.
#[derive(Default)]
struct RefusesReads {
    asked: Mutex<Vec<String>>,
}

#[async_trait]
impl ApprovalGate for RefusesReads {
    async fn authorize(
        &self,
        request: &ApprovalRequest,
    ) -> Result<ApprovalDecision, ApprovalError> {
        let name = request.call.name.to_string();
        self.asked.lock().unwrap().push(name.clone());
        Ok(if name == "read_file" {
            ApprovalDecision::Deny {
                reason: "the operator's rule forbids it".into(),
            }
        } else {
            ApprovalDecision::Approve
        })
    }
}

#[tokio::test]
async fn the_sub_agents_calls_are_decided_by_the_parents_gate() {
    let provider = ScriptedProvider::new(vec![
        spawn("s1", TASK),
        read("r1"),
        assistant("I was not allowed to read it."),
        assistant("The file could not be read."),
    ]);
    let gate = Arc::new(RefusesReads::default());
    let events = Arc::new(RecordingSink::default());
    let mut wiring = Wiring::new(provider);
    wiring.approval = gate.clone();
    wiring.events = events.clone();
    let agents = wiring.build(tools());
    let mut session = Session::new("parent");

    agents
        .parent
        .run(&mut session, "Explain src/lib.rs")
        .await
        .unwrap();

    assert_eq!(*gate.asked.lock().unwrap(), ["spawn_agent", "read_file"]);
    assert_eq!(
        results(&session)[0].content,
        "I was not allowed to read it."
    );
    let events = events.events.lock().unwrap();
    assert!(
        events.iter().any(|event| matches!(
            event,
            AgentEvent::Nested { parent_call_id, event }
                if parent_call_id.as_str() == "s1"
                    && matches!(event.as_ref(), AgentEvent::ToolCallDenied { .. })
        )),
        "the denial is reported as the sub-agent's: {events:?}"
    );
}

#[tokio::test]
async fn the_sub_agents_events_reach_the_parents_sink_marked_as_its_own() {
    let provider = ScriptedProvider::new(delegation());
    let events = Arc::new(RecordingSink::default());
    let mut wiring = Wiring::new(provider);
    wiring.events = events.clone();
    let agents = wiring.build(tools());

    agents
        .parent
        .run(&mut Session::new("parent"), "Explain src/lib.rs")
        .await
        .unwrap();

    let events = events.events.lock().unwrap();
    let nested: Vec<&AgentEvent> = events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::Nested {
                parent_call_id,
                event,
            } => {
                assert_eq!(parent_call_id.as_str(), "s1");
                Some(event.as_ref())
            }
            _ => None,
        })
        .collect();
    assert!(
        nested.iter().any(|event| matches!(
            event,
            AgentEvent::ToolCallStarted { call, .. } if call.name.as_str() == "read_file"
        )),
        "{nested:?}"
    );
    // The child's answer is not the turn's answer.
    let answers: Vec<&String> = events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::AssistantMessage { text } => Some(text),
            _ => None,
        })
        .collect();
    assert_eq!(answers, ["Done: src/lib.rs defines the body."]);
    assert!(
        !events.iter().any(|event| matches!(
            event,
            AgentEvent::ToolCallStarted { call, .. } if call.name.as_str() == "read_file"
        )),
        "the sub-agent's call was reported as the parent's"
    );
}

#[tokio::test]
async fn sub_agents_started_together_each_get_a_session_named_after_their_call() {
    // Identical arguments on purpose: only the call tells the two apart.
    let both = Message::assistant(vec![
        ContentBlock::ToolCall(agent_domain::model::tool::ToolCall::new(
            agent_domain::model::tool::ToolCallId::new("s1"),
            agent_domain::model::tool::ToolName::new("spawn_agent").unwrap(),
            json!({"task": TASK}),
        )),
        ContentBlock::ToolCall(agent_domain::model::tool::ToolCall::new(
            agent_domain::model::tool::ToolCallId::new("s2"),
            agent_domain::model::tool::ToolName::new("spawn_agent").unwrap(),
            json!({"task": TASK}),
        )),
    ]);
    let provider = ScriptedProvider::new(vec![
        response(both, StopReason::ToolUse),
        assistant("first"),
        assistant("second"),
        assistant("Both done."),
    ]);
    let log = Arc::new(RecordingLog::default());
    let mut wiring = Wiring::new(provider);
    wiring.log = log.clone();
    let agents = wiring.build(tools());
    let mut session = Session::new("parent");

    agents
        .parent
        .run(&mut session, "Explain src/lib.rs")
        .await
        .unwrap();

    let contents: Vec<String> = results(&session).into_iter().map(|r| r.content).collect();
    assert_eq!(contents, ["first", "second"]);
    let mut sessions: Vec<String> = log
        .seen
        .lock()
        .unwrap()
        .iter()
        .map(|(session, _)| session.clone())
        .collect();
    sessions.dedup();
    sessions.sort();
    sessions.dedup();
    assert_eq!(sessions, ["parent", "parent.s1", "parent.s2"]);
}

#[tokio::test]
async fn spawn_agent_refuses_to_run_outside_a_dispatched_call() {
    use agent_domain::ports::tool::Tool as _;

    let provider = ScriptedProvider::new(Vec::new());
    let child = AgentLoop::new(
        AgentDependencies {
            llm: provider.clone(),
            tools: Arc::new(ToolRegistry::new()),
            approval: Arc::new(AlwaysApprove),
            events: Arc::new(agent_domain::ports::events::NullEventSink),
            context: Arc::new(FakeContext),
            prompt: Arc::new(SubagentPromptBuilder),
            log: Arc::new(agent_domain::ports::conversation_log::NullConversationLog),
        },
        AgentLoopConfig::default(),
    );
    let tool = SpawnAgentTool::new(
        Arc::new(child),
        SubagentLimits {
            max_answer_bytes: 64,
        },
    );

    let error = tool.execute(json!({"task": TASK})).await.unwrap_err();

    assert!(error.to_string().contains("yourself"), "{error}");
    assert!(provider.requests().is_empty(), "a sub-agent ran unnamed");
}

#[tokio::test]
async fn a_long_answer_is_cut_to_the_limit_and_says_so() {
    let provider = ScriptedProvider::new(vec![
        spawn("s1", TASK),
        assistant(&"x".repeat(5_000)),
        assistant("ok"),
    ]);
    let agents = Wiring::new(provider).build(tools());
    let mut session = Session::new("parent");

    agents
        .parent
        .run(&mut session, "Explain src/lib.rs")
        .await
        .unwrap();

    let result = &results(&session)[0];
    assert!(!result.is_error);
    assert!(result.content.len() < 1_200, "{}", result.content.len());
    assert!(
        result.content.contains("truncated at 1024 bytes"),
        "{}",
        result.content
    );
}

#[tokio::test]
async fn an_empty_task_is_refused_with_what_to_write() {
    let provider = ScriptedProvider::new(vec![spawn("s1", "   "), assistant("ok")]);
    let agents = Wiring::new(provider.clone()).build(tools());
    let mut session = Session::new("parent");

    agents
        .parent
        .run(&mut session, "Explain src/lib.rs")
        .await
        .unwrap();

    let result = &results(&session)[0];
    assert!(result.is_error);
    assert!(
        result.content.contains("`task` is empty"),
        "{}",
        result.content
    );
    assert_eq!(provider.requests().len(), 2, "no sub-agent was started");
}
