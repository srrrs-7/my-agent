//! Fakes and helpers shared by the agent-loop test files.
//!
//! Every port is faked here, so the loop runs with no HTTP, no filesystem and
//! no runtime timers. Each test file compiles this module on its own and uses
//! only part of it, hence the `dead_code` allowance.

#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use agent_application::agent::{
    AgentDependencies, AgentLoop, AgentLoopConfig, DefaultPromptBuilder, DispatchConfig, Session,
    ToolDispatcher,
};
use agent_application::tools::ToolRegistry;
use agent_application::tools::file::{ReadFileTool, WriteFileTool};
use agent_application::tools::plan::TodoWriteTool;
use agent_domain::error::{ApprovalError, FsError, LlmError};
use agent_domain::model::context::ContextSnapshot;
use agent_domain::model::conversation::Conversation;
use agent_domain::model::llm::{
    ChatRequest, ChatResponse, ModelId, ProviderCapabilities, ProviderId, StopReason, TokenUsage,
};
use agent_domain::model::message::{ContentBlock, Message};
use agent_domain::model::tool::{ToolCall, ToolCallId, ToolName, ToolResult};
use agent_domain::model::workspace::{WorkspacePath, WorkspaceRoot};
use agent_domain::ports::approval::{ApprovalDecision, ApprovalGate, ApprovalRequest};
use agent_domain::ports::context::ContextProvider;
use agent_domain::ports::conversation_log::{ConversationLog, NullConversationLog};
use agent_domain::ports::events::{AgentEvent, EventSink, NullEventSink};
use agent_domain::ports::file_system::{DirEntry, EntryKind, FileSystem};
use agent_domain::ports::llm::{LlmProvider, StreamEvent};
use async_trait::async_trait;
use serde_json::json;

/// Replays a fixed script of assistant turns and records what it was asked.
pub struct ScriptedProvider {
    pub script: Mutex<Vec<ChatResponse>>,
    pub seen: Mutex<Vec<ChatRequest>>,
}

impl ScriptedProvider {
    pub fn new(script: Vec<ChatResponse>) -> Arc<Self> {
        Arc::new(Self {
            script: Mutex::new(script),
            seen: Mutex::new(Vec::new()),
        })
    }

    pub fn requests(&self) -> Vec<ChatRequest> {
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
pub struct MemoryFileSystem {
    pub files: Mutex<HashMap<String, String>>,
    /// Files that are not text; only `read_bytes` returns them whole.
    pub blobs: Mutex<HashMap<String, Vec<u8>>>,
}

impl MemoryFileSystem {
    pub fn with(files: &[(&str, &str)]) -> Arc<Self> {
        Arc::new(Self {
            files: Mutex::new(
                files
                    .iter()
                    .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                    .collect(),
            ),
            blobs: Mutex::default(),
        })
    }

    /// Adds a binary file.
    pub fn put_bytes(&self, path: &str, bytes: &[u8]) {
        self.blobs
            .lock()
            .unwrap()
            .insert(path.to_string(), bytes.to_vec());
    }

    pub fn get(&self, path: &str) -> Option<String> {
        self.files.lock().unwrap().get(path).cloned()
    }
}

#[async_trait]
impl FileSystem for MemoryFileSystem {
    async fn read_to_string(&self, path: &WorkspacePath) -> Result<String, FsError> {
        let blob = self.blobs.lock().unwrap().get(&path.display()).cloned();
        if let Some(blob) = blob {
            return String::from_utf8(blob).map_err(|_| FsError::NotUtf8 {
                path: path.display(),
            });
        }
        self.files
            .lock()
            .unwrap()
            .get(&path.display())
            .cloned()
            .ok_or_else(|| FsError::NotFound {
                path: path.display(),
            })
    }

    async fn read_bytes(&self, path: &WorkspacePath, limit: u64) -> Result<Vec<u8>, FsError> {
        let blob = self.blobs.lock().unwrap().get(&path.display()).cloned();
        let bytes = match blob {
            Some(blob) => blob,
            None => self.read_to_string(path).await?.into_bytes(),
        };
        if bytes.len() as u64 > limit {
            return Err(FsError::TooLarge {
                path: path.display(),
                actual: bytes.len() as u64,
                limit,
            });
        }
        Ok(bytes)
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

    async fn remove_file(&self, path: &WorkspacePath) -> Result<(), FsError> {
        self.files
            .lock()
            .unwrap()
            .remove(&path.display())
            .map(|_| ())
            .ok_or_else(|| FsError::NotFound {
                path: path.display(),
            })
    }

    /// No links in memory: every path is its own file.
    async fn resolve_real(&self, path: &WorkspacePath) -> Result<WorkspacePath, FsError> {
        Ok(path.clone())
    }
}

pub struct FakeContext;

#[async_trait]
impl ContextProvider for FakeContext {
    async fn snapshot(&self) -> Result<ContextSnapshot, FsError> {
        Ok(ContextSnapshot {
            workspace_root: "/workspace".into(),
            os: "linux".into(),
            today: "2026-08-13".into(),
            is_git_repository: true,
            project_instructions: None,
            nested_instructions: Vec::new(),
            directory_overview: vec!["src/".into()],
        })
    }
}

/// Context whose directory overview is whatever the file system holds right
/// now - the property of the real provider that a write can change.
pub struct ListingContext(pub Arc<MemoryFileSystem>);

#[async_trait]
impl ContextProvider for ListingContext {
    async fn snapshot(&self) -> Result<ContextSnapshot, FsError> {
        let mut overview: Vec<String> = self.0.files.lock().unwrap().keys().cloned().collect();
        overview.sort();
        Ok(ContextSnapshot {
            directory_overview: overview,
            ..FakeContext.snapshot().await?
        })
    }
}

pub struct AlwaysApprove;

#[async_trait]
impl ApprovalGate for AlwaysApprove {
    async fn reauthorize_write(
        &self,
        _request: &ApprovalRequest,
        _writes: &[agent_domain::model::tool::PlannedWrite],
    ) -> Result<ApprovalDecision, ApprovalError> {
        Ok(ApprovalDecision::Approve)
    }

    async fn authorize(&self, _: &ApprovalRequest) -> Result<ApprovalDecision, ApprovalError> {
        Ok(ApprovalDecision::Approve)
    }
}

pub struct AlwaysDeny;

#[async_trait]
impl ApprovalGate for AlwaysDeny {
    async fn reauthorize_write(
        &self,
        _request: &ApprovalRequest,
        _writes: &[agent_domain::model::tool::PlannedWrite],
    ) -> Result<ApprovalDecision, ApprovalError> {
        Ok(ApprovalDecision::Approve)
    }

    async fn authorize(&self, _: &ApprovalRequest) -> Result<ApprovalDecision, ApprovalError> {
        Ok(ApprovalDecision::Deny {
            reason: "not this time".into(),
        })
    }
}

#[derive(Default)]
pub struct RecordingSink {
    pub events: Mutex<Vec<AgentEvent>>,
}

impl EventSink for RecordingSink {
    fn emit(&self, event: AgentEvent) {
        self.events.lock().unwrap().push(event);
    }
}

pub fn assistant(text: &str) -> ChatResponse {
    response(Message::assistant_text(text), StopReason::EndTurn)
}

pub fn tool_use(id: &str, tool: &str, arguments: serde_json::Value) -> ChatResponse {
    let call = ToolCall::new(ToolCallId::new(id), ToolName::new(tool).unwrap(), arguments);
    response(
        Message::assistant(vec![ContentBlock::ToolCall(call)]),
        StopReason::ToolUse,
    )
}

pub fn response(message: Message, stop_reason: StopReason) -> ChatResponse {
    ChatResponse {
        message,
        stop_reason,
        usage: TokenUsage {
            input_tokens: 10,
            output_tokens: 5,
            ..TokenUsage::default()
        },
        model: ModelId::new("fake"),
        provider: ProviderId::new("scripted"),
    }
}

pub fn root() -> Arc<WorkspaceRoot> {
    Arc::new(WorkspaceRoot::new("/workspace").unwrap())
}

pub fn registry(file_system: Arc<MemoryFileSystem>) -> Arc<ToolRegistry> {
    Arc::new(
        ToolRegistry::new()
            .with(Arc::new(ReadFileTool::new(file_system.clone(), root())))
            .with(Arc::new(WriteFileTool::new(file_system, root()))),
    )
}

pub fn loop_with(
    provider: Arc<dyn LlmProvider>,
    tools: Arc<ToolRegistry>,
    approval: Arc<dyn ApprovalGate>,
    events: Arc<dyn EventSink>,
    max_iterations: u32,
) -> AgentLoop {
    loop_with_config(
        provider,
        tools,
        approval,
        events,
        AgentLoopConfig {
            max_iterations,
            ..AgentLoopConfig::default()
        },
    )
}

pub fn loop_with_config(
    provider: Arc<dyn LlmProvider>,
    tools: Arc<ToolRegistry>,
    approval: Arc<dyn ApprovalGate>,
    events: Arc<dyn EventSink>,
    config: AgentLoopConfig,
) -> AgentLoop {
    AgentLoop::new(
        AgentDependencies {
            llm: provider,
            tools,
            approval,
            events,
            context: Arc::new(FakeContext),
            prompt: Arc::new(DefaultPromptBuilder),
            log: Arc::new(NullConversationLog),
        },
        config,
    )
}

pub fn delta(text: &str) -> Result<StreamEvent, LlmError> {
    Ok(StreamEvent::TextDelta(text.to_string()))
}

#[derive(Default)]
pub struct RecordingLog {
    pub seen: Mutex<Vec<(String, Message)>>,
    /// Every rewind mark, as `(session, seq)`.
    pub rewinds: Mutex<Vec<(String, u64)>>,
}

#[async_trait]
impl ConversationLog for RecordingLog {
    async fn append(&self, session_id: &str, messages: &[Message]) -> Result<(), FsError> {
        let mut seen = self.seen.lock().unwrap();
        seen.extend(
            messages
                .iter()
                .map(|message| (session_id.to_string(), message.clone())),
        );
        Ok(())
    }

    async fn rewind(&self, session_id: &str, seq: u64) -> Result<(), FsError> {
        self.rewinds
            .lock()
            .unwrap()
            .push((session_id.to_string(), seq));
        Ok(())
    }
}

pub fn loop_with_log(
    provider: Arc<dyn LlmProvider>,
    tools: Arc<ToolRegistry>,
    log: Arc<dyn ConversationLog>,
) -> AgentLoop {
    AgentLoop::new(
        AgentDependencies {
            llm: provider,
            tools,
            approval: Arc::new(AlwaysApprove),
            events: Arc::new(NullEventSink),
            context: Arc::new(FakeContext),
            prompt: Arc::new(DefaultPromptBuilder),
            log,
        },
        AgentLoopConfig::default(),
    )
}

/// A history well past any small budget, ending on a complete tool turn.
pub fn crowded_session(id: &str) -> Session {
    let mut session = Session::new(id);
    session
        .conversation
        .push(Message::user("build a parser, and keep it dependency-free"));
    for turn in 0..6 {
        let call = ToolCall::new(
            ToolCallId::new(format!("c{turn}")),
            ToolName::new("read_file").unwrap(),
            json!({"path": format!("src/{turn}.rs")}),
        );
        session
            .conversation
            .push(Message::assistant(vec![ContentBlock::ToolCall(
                call.clone(),
            )]));
        session
            .conversation
            .push(Message::tool_results(vec![ToolResult::ok(
                &call,
                "x".repeat(4_000),
            )]));
    }
    session
}

pub fn compacting_config(max_history_bytes: usize) -> AgentLoopConfig {
    AgentLoopConfig {
        max_iterations: 3,
        max_history_bytes,
        compact: true,
        compact_keep_recent: 2,
        ..AgentLoopConfig::default()
    }
}

/// Tool results in `messages` whose call is nowhere to be found.
///
/// This is invariant §4 seen from the provider's side: it is what makes a
/// request get rejected outright, so it is worth checking on the bytes that
/// would actually be sent rather than on the conversation that produced them.
pub fn orphaned_results(messages: &[Message]) -> Vec<String> {
    let calls: std::collections::HashSet<&str> = messages
        .iter()
        .flat_map(Message::tool_calls)
        .map(|call| call.id.as_str())
        .collect();

    messages
        .iter()
        .flat_map(|message| message.content.iter())
        .filter_map(|block| match block {
            ContentBlock::ToolResult(result) => Some(result.call_id.as_str()),
            _ => None,
        })
        .filter(|id| !calls.contains(id))
        .map(str::to_string)
        .collect()
}

/// What the pinned note opens with - distinctive enough to count.
pub const NOTE: &str = "[Agent note";

pub fn todos(steps: &[(&str, &str)]) -> serde_json::Value {
    json!({
        "todos": steps
            .iter()
            .map(|(content, status)| json!({"content": content, "status": status}))
            .collect::<Vec<_>>()
    })
}

pub fn registry_with_plan(file_system: Arc<MemoryFileSystem>) -> Arc<ToolRegistry> {
    Arc::new(
        ToolRegistry::new()
            .with(Arc::new(ReadFileTool::new(file_system.clone(), root())))
            .with(Arc::new(WriteFileTool::new(file_system, root())))
            .with(Arc::new(TodoWriteTool::new())),
    )
}

/// A recorded history that wrote `steps` as its plan early on and then grew
/// well past any small budget - the shape of a long task about to be
/// shortened.
pub fn planned_record(steps: &[(&str, &str)]) -> Conversation {
    let crowded = crowded_session("unused").conversation.into_messages();
    let call = ToolCall::new(
        ToolCallId::new("plan-0"),
        ToolName::new("todo_write").unwrap(),
        todos(steps),
    );
    let mut messages = vec![
        crowded[0].clone(),
        Message::assistant(vec![ContentBlock::ToolCall(call.clone())]),
        Message::tool_results(vec![ToolResult::ok(&call, "Plan updated")]),
    ];
    messages.extend(crowded.into_iter().skip(1));
    Conversation::from_messages(messages)
}

pub const UNFINISHED: &[(&str, &str)] = &[
    ("read the grammar", "completed"),
    ("write the lexer", "in_progress"),
    ("write the parser", "pending"),
];

/// What the model is told about a call the interrupt reached before it ran.
pub const NOT_STARTED: &str = "Not executed";

pub fn calls(ids_and_tools: &[(&str, &str, serde_json::Value)]) -> Vec<ToolCall> {
    ids_and_tools
        .iter()
        .map(|(id, tool, arguments)| {
            ToolCall::new(
                ToolCallId::new(*id),
                ToolName::new(*tool).unwrap(),
                arguments.clone(),
            )
        })
        .collect()
}

pub fn dispatcher(
    tools: Arc<ToolRegistry>,
    approval: Arc<dyn ApprovalGate>,
    events: Arc<dyn EventSink>,
) -> ToolDispatcher {
    ToolDispatcher::new(
        tools,
        approval,
        events,
        DispatchConfig {
            max_tool_output_bytes: 64 * 1024,
            parallel_read_only_tools: true,
        },
    )
}
