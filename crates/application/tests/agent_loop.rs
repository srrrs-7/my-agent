//! End-to-end tests of the agent loop with every port faked.
//!
//! No HTTP, no filesystem, no runtime timers - which is the payoff of keeping
//! the use-case layer free of infrastructure. Each test scripts what the model
//! "says" and asserts on what the loop does with it.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use agent_application::agent::{
    AgentDependencies, AgentLoop, AgentLoopConfig, DefaultPromptBuilder, Session,
};
use agent_application::tools::ToolRegistry;
use agent_application::tools::file::{ReadFileTool, WriteFileTool};
use agent_domain::error::{ApprovalError, FsError, LlmError};
use agent_domain::model::context::ContextSnapshot;
use agent_domain::model::llm::{
    ChatRequest, ChatResponse, ModelId, ProviderCapabilities, ProviderId, StopReason, TokenUsage,
};
use agent_domain::model::message::{ContentBlock, Message};
use agent_domain::model::tool::{ToolCall, ToolCallId, ToolName};
use agent_domain::model::workspace::{WorkspacePath, WorkspaceRoot};
use agent_domain::ports::approval::{ApprovalDecision, ApprovalGate, ApprovalRequest};
use agent_domain::ports::context::ContextProvider;
use agent_domain::ports::events::{AgentEvent, EventSink, FinishReason, NullEventSink};
use agent_domain::ports::file_system::{DirEntry, EntryKind, FileSystem};
use agent_domain::ports::llm::LlmProvider;
use async_trait::async_trait;
use serde_json::json;

// --- fakes -------------------------------------------------------------------

/// Replays a fixed script of assistant turns and records what it was asked.
struct ScriptedProvider {
    script: Mutex<Vec<ChatResponse>>,
    seen: Mutex<Vec<ChatRequest>>,
}

impl ScriptedProvider {
    fn new(script: Vec<ChatResponse>) -> Arc<Self> {
        Arc::new(Self {
            script: Mutex::new(script),
            seen: Mutex::new(Vec::new()),
        })
    }

    fn requests(&self) -> Vec<ChatRequest> {
        self.seen.lock().unwrap().clone()
    }
}

#[async_trait]
impl LlmProvider for ScriptedProvider {
    fn id(&self) -> ProviderId {
        ProviderId::new("scripted")
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities::default()
    }

    async fn chat(&self, request: ChatRequest) -> Result<ChatResponse, LlmError> {
        self.seen.lock().unwrap().push(request);
        let mut script = self.script.lock().unwrap();
        if script.is_empty() {
            // Keeps a runaway loop from hanging the test suite.
            return Ok(assistant("(script exhausted)"));
        }
        Ok(script.remove(0))
    }
}

#[derive(Default)]
struct MemoryFileSystem {
    files: Mutex<HashMap<String, String>>,
}

impl MemoryFileSystem {
    fn with(files: &[(&str, &str)]) -> Arc<Self> {
        Arc::new(Self {
            files: Mutex::new(
                files
                    .iter()
                    .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                    .collect(),
            ),
        })
    }

    fn get(&self, path: &str) -> Option<String> {
        self.files.lock().unwrap().get(path).cloned()
    }
}

#[async_trait]
impl FileSystem for MemoryFileSystem {
    async fn read_to_string(&self, path: &WorkspacePath) -> Result<String, FsError> {
        self.files
            .lock()
            .unwrap()
            .get(&path.display())
            .cloned()
            .ok_or_else(|| FsError::NotFound {
                path: path.display(),
            })
    }

    async fn write(&self, path: &WorkspacePath, contents: &str) -> Result<(), FsError> {
        self.files
            .lock()
            .unwrap()
            .insert(path.display(), contents.to_string());
        Ok(())
    }

    async fn list_dir(&self, _path: &WorkspacePath) -> Result<Vec<DirEntry>, FsError> {
        Ok(self
            .files
            .lock()
            .unwrap()
            .keys()
            .map(|key| DirEntry {
                path: WorkspacePath::parse(key).unwrap(),
                kind: EntryKind::File,
                size_bytes: 0,
            })
            .collect())
    }

    async fn exists(&self, path: &WorkspacePath) -> Result<bool, FsError> {
        Ok(self.files.lock().unwrap().contains_key(&path.display()))
    }
}

struct FakeContext;

#[async_trait]
impl ContextProvider for FakeContext {
    async fn snapshot(&self) -> Result<ContextSnapshot, FsError> {
        Ok(ContextSnapshot {
            workspace_root: "/workspace".into(),
            os: "linux".into(),
            today: "2026-08-13".into(),
            is_git_repository: true,
            project_instructions: None,
            directory_overview: vec!["src/".into()],
        })
    }
}

struct AlwaysApprove;

#[async_trait]
impl ApprovalGate for AlwaysApprove {
    async fn authorize(&self, _: &ApprovalRequest) -> Result<ApprovalDecision, ApprovalError> {
        Ok(ApprovalDecision::Approve)
    }
}

struct AlwaysDeny;

#[async_trait]
impl ApprovalGate for AlwaysDeny {
    async fn authorize(&self, _: &ApprovalRequest) -> Result<ApprovalDecision, ApprovalError> {
        Ok(ApprovalDecision::Deny {
            reason: "not this time".into(),
        })
    }
}

#[derive(Default)]
struct RecordingSink {
    events: Mutex<Vec<AgentEvent>>,
}

impl EventSink for RecordingSink {
    fn emit(&self, event: AgentEvent) {
        self.events.lock().unwrap().push(event);
    }
}

// --- helpers -----------------------------------------------------------------

fn assistant(text: &str) -> ChatResponse {
    response(Message::assistant_text(text), StopReason::EndTurn)
}

fn tool_use(id: &str, tool: &str, arguments: serde_json::Value) -> ChatResponse {
    let call = ToolCall::new(ToolCallId::new(id), ToolName::new(tool).unwrap(), arguments);
    response(
        Message::assistant(vec![ContentBlock::ToolCall(call)]),
        StopReason::ToolUse,
    )
}

fn response(message: Message, stop_reason: StopReason) -> ChatResponse {
    ChatResponse {
        message,
        stop_reason,
        usage: TokenUsage {
            input_tokens: 10,
            output_tokens: 5,
        },
        model: ModelId::new("fake"),
        provider: ProviderId::new("scripted"),
    }
}

fn root() -> Arc<WorkspaceRoot> {
    Arc::new(WorkspaceRoot::new("/workspace").unwrap())
}

fn registry(file_system: Arc<MemoryFileSystem>) -> Arc<ToolRegistry> {
    Arc::new(
        ToolRegistry::new()
            .with(Arc::new(ReadFileTool::new(file_system.clone(), root())))
            .with(Arc::new(WriteFileTool::new(file_system, root()))),
    )
}

fn loop_with(
    provider: Arc<dyn LlmProvider>,
    tools: Arc<ToolRegistry>,
    approval: Arc<dyn ApprovalGate>,
    events: Arc<dyn EventSink>,
    max_iterations: u32,
) -> AgentLoop {
    AgentLoop::new(
        AgentDependencies {
            llm: provider,
            tools,
            approval,
            events,
            context: Arc::new(FakeContext),
            prompt: Arc::new(DefaultPromptBuilder),
        },
        AgentLoopConfig {
            max_iterations,
            ..AgentLoopConfig::default()
        },
    )
}

// --- tests -------------------------------------------------------------------

#[tokio::test]
async fn reads_a_file_then_answers() {
    let file_system = MemoryFileSystem::with(&[("src/main.rs", "fn main() {}\n")]);
    let provider = ScriptedProvider::new(vec![
        tool_use("c1", "read_file", json!({"path": "src/main.rs"})),
        assistant("It is an empty main function."),
    ]);

    let agent = loop_with(
        provider.clone(),
        registry(file_system),
        Arc::new(AlwaysApprove),
        Arc::new(NullEventSink),
        10,
    );

    let mut session = Session::new("s1");
    let outcome = agent
        .run(&mut session, "what does src/main.rs do?")
        .await
        .unwrap();

    assert_eq!(outcome.final_text, "It is an empty main function.");
    assert_eq!(outcome.iterations, 2);
    assert_eq!(outcome.reason, FinishReason::Completed);
    assert_eq!(
        outcome.usage.total(),
        30,
        "usage accumulates across iterations"
    );

    // The second request must carry the tool result the model asked for.
    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    let last = requests.last().unwrap();
    assert!(
        last.messages.iter().any(|message| message
            .content
            .iter()
            .any(|block| matches!(block, ContentBlock::ToolResult(result)
                if result.content.contains("fn main()")))),
        "the file contents must be fed back to the model"
    );
    assert!(last.system.as_ref().unwrap().contains("/workspace"));
    assert_eq!(
        last.tools.len(),
        2,
        "tool definitions are advertised every turn"
    );
}

#[tokio::test]
async fn a_write_lands_in_the_filesystem() {
    let file_system = MemoryFileSystem::with(&[]);
    let provider = ScriptedProvider::new(vec![
        tool_use(
            "c1",
            "write_file",
            json!({"path": "notes.md", "content": "hello"}),
        ),
        assistant("Created notes.md."),
    ]);

    let agent = loop_with(
        provider,
        registry(file_system.clone()),
        Arc::new(AlwaysApprove),
        Arc::new(NullEventSink),
        10,
    );

    let mut session = Session::new("s2");
    agent.run(&mut session, "create notes.md").await.unwrap();

    assert_eq!(file_system.get("notes.md").as_deref(), Some("hello"));
}

#[tokio::test]
async fn an_unknown_tool_is_reported_back_instead_of_aborting() {
    let file_system = MemoryFileSystem::with(&[]);
    let provider = ScriptedProvider::new(vec![
        tool_use("c1", "delete_everything", json!({})),
        assistant("Sorry, I used a tool that does not exist."),
    ]);

    let agent = loop_with(
        provider.clone(),
        registry(file_system),
        Arc::new(AlwaysApprove),
        Arc::new(NullEventSink),
        10,
    );

    let mut session = Session::new("s3");
    let outcome = agent.run(&mut session, "delete it all").await.unwrap();

    assert_eq!(outcome.reason, FinishReason::Completed);
    let last = provider.requests().last().unwrap().clone();
    let fed_back = last
        .messages
        .iter()
        .flat_map(|message| message.content.iter())
        .filter_map(|block| match block {
            ContentBlock::ToolResult(result) => Some(result),
            _ => None,
        })
        .next()
        .expect("a tool result must be produced even for an unknown tool");

    assert!(fed_back.is_error);
    assert!(fed_back.content.contains("Unknown tool"));
    assert!(
        fed_back.content.contains("read_file"),
        "the model is told what it may use"
    );
}

#[tokio::test]
async fn a_failing_tool_call_is_recoverable() {
    let file_system = MemoryFileSystem::with(&[]);
    let provider = ScriptedProvider::new(vec![
        tool_use("c1", "read_file", json!({"path": "missing.rs"})),
        assistant("That file does not exist."),
    ]);

    let agent = loop_with(
        provider.clone(),
        registry(file_system),
        Arc::new(AlwaysApprove),
        Arc::new(NullEventSink),
        10,
    );

    let mut session = Session::new("s4");
    let outcome = agent.run(&mut session, "read missing.rs").await.unwrap();

    assert_eq!(outcome.final_text, "That file does not exist.");
    assert_eq!(outcome.reason, FinishReason::Completed);
}

#[tokio::test]
async fn a_denied_call_tells_the_model_why() {
    let file_system = MemoryFileSystem::with(&[]);
    let provider = ScriptedProvider::new(vec![
        tool_use("c1", "write_file", json!({"path": "a.txt", "content": "x"})),
        assistant("Understood, I will not write the file."),
    ]);

    let events = Arc::new(RecordingSink::default());
    let agent = loop_with(
        provider.clone(),
        registry(file_system.clone()),
        Arc::new(AlwaysDeny),
        events.clone(),
        10,
    );

    let mut session = Session::new("s5");
    agent.run(&mut session, "write a.txt").await.unwrap();

    assert!(
        file_system.get("a.txt").is_none(),
        "a denied call must not run"
    );
    assert!(
        events
            .events
            .lock()
            .unwrap()
            .iter()
            .any(|event| matches!(event, AgentEvent::ToolCallDenied { .. }))
    );

    let last = provider.requests().last().unwrap().clone();
    assert!(
        last.messages
            .iter()
            .flat_map(|m| m.content.iter())
            .any(|block| matches!(
                block,
                ContentBlock::ToolResult(result) if result.content.contains("not this time")
            ))
    );
}

#[tokio::test]
async fn the_iteration_budget_is_enforced() {
    // A model that never stops asking for tools.
    let provider = ScriptedProvider::new(
        (0..10)
            .map(|i| tool_use(&format!("c{i}"), "read_file", json!({"path": "a"})))
            .collect(),
    );

    let agent = loop_with(
        provider.clone(),
        registry(MemoryFileSystem::with(&[("a", "x")])),
        Arc::new(AlwaysApprove),
        Arc::new(NullEventSink),
        3,
    );

    let mut session = Session::new("s6");
    let outcome = agent.run(&mut session, "loop forever").await.unwrap();

    assert_eq!(outcome.reason, FinishReason::MaxIterations { limit: 3 });
    assert_eq!(outcome.iterations, 3);
    assert_eq!(
        provider.requests().len(),
        3,
        "the provider is not called again after the budget"
    );
    assert!(!outcome.is_complete());
}

#[tokio::test]
async fn parallel_read_only_calls_all_produce_results_in_order() {
    let file_system = MemoryFileSystem::with(&[("a.rs", "AAA"), ("b.rs", "BBB")]);

    let calls = Message::assistant(vec![
        ContentBlock::ToolCall(ToolCall::new(
            ToolCallId::new("c1"),
            ToolName::new("read_file").unwrap(),
            json!({"path": "a.rs"}),
        )),
        ContentBlock::ToolCall(ToolCall::new(
            ToolCallId::new("c2"),
            ToolName::new("read_file").unwrap(),
            json!({"path": "b.rs"}),
        )),
    ]);

    let provider = ScriptedProvider::new(vec![
        response(calls, StopReason::ToolUse),
        assistant("Both read."),
    ]);

    let agent = loop_with(
        provider.clone(),
        registry(file_system),
        Arc::new(AlwaysApprove),
        Arc::new(NullEventSink),
        10,
    );

    let mut session = Session::new("s7");
    agent.run(&mut session, "read both").await.unwrap();

    let last = provider.requests().last().unwrap().clone();
    let results: Vec<_> = last
        .messages
        .iter()
        .flat_map(|message| message.content.iter())
        .filter_map(|block| match block {
            ContentBlock::ToolResult(result) => Some(result),
            _ => None,
        })
        .collect();

    assert_eq!(results.len(), 2, "every call must be answered");
    assert_eq!(
        results[0].call_id.as_str(),
        "c1",
        "order follows the model's request"
    );
    assert_eq!(results[1].call_id.as_str(), "c2");
    assert!(results[0].content.contains("AAA"));
    assert!(results[1].content.contains("BBB"));
}

#[tokio::test]
async fn the_session_history_survives_multiple_turns() {
    let provider = ScriptedProvider::new(vec![assistant("first"), assistant("second")]);
    let agent = loop_with(
        provider.clone(),
        registry(MemoryFileSystem::with(&[])),
        Arc::new(AlwaysApprove),
        Arc::new(NullEventSink),
        5,
    );

    let mut session = Session::new("s8");
    agent.run(&mut session, "one").await.unwrap();
    agent.run(&mut session, "two").await.unwrap();

    // user, assistant, user, assistant
    assert_eq!(session.conversation.len(), 4);
    let second_request = &provider.requests()[1];
    assert_eq!(
        second_request.messages.len(),
        3,
        "the earlier turn is still in context"
    );
}
