//! The evaluation use cases with every port faked: checks against an
//! in-memory filesystem and a scripted command runner, and the metrics a run
//! of the real loop leaves in a [`MetricsSink`].

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent_application::agent::{
    AgentDependencies, AgentLoop, AgentLoopConfig, DefaultPromptBuilder, Session,
};
use agent_application::eval::{Check, MetricsSink, RunMetrics, verify};
use agent_application::tools::ToolRegistry;
use agent_application::tools::file::{ReadFileTool, WriteFileTool};
use agent_domain::error::{ApprovalError, CommandError, FsError, LlmError};
use agent_domain::model::context::ContextSnapshot;
use agent_domain::model::llm::{
    ChatRequest, ChatResponse, ModelId, ProviderCapabilities, ProviderId, StopReason, TokenUsage,
};
use agent_domain::model::message::{ContentBlock, Message};
use agent_domain::model::tool::{ToolCall, ToolCallId, ToolName};
use agent_domain::model::workspace::{WorkspacePath, WorkspaceRoot};
use agent_domain::ports::approval::{ApprovalDecision, ApprovalGate, ApprovalRequest};
use agent_domain::ports::command::{CommandOutput, CommandRequest, CommandRunner, SandboxKind};
use agent_domain::ports::context::ContextProvider;
use agent_domain::ports::conversation_log::NullConversationLog;
use agent_domain::ports::events::{AgentEvent, EventSink};
use agent_domain::ports::file_system::{DirEntry, FileSystem};
use agent_domain::ports::llm::LlmProvider;
use async_trait::async_trait;
use serde_json::json;

// --- fakes -------------------------------------------------------------------

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
        Ok(Vec::new())
    }

    async fn exists(&self, path: &WorkspacePath) -> Result<bool, FsError> {
        Ok(self.files.lock().unwrap().contains_key(&path.display()))
    }

    /// No links in memory: every path is its own file.
    async fn resolve_real(&self, path: &WorkspacePath) -> Result<WorkspacePath, FsError> {
        Ok(path.clone())
    }
}

/// Answers every command with a fixed exit code and records what it ran.
struct ScriptedRunner {
    exit_code: i32,
    ran: Mutex<Vec<CommandRequest>>,
}

impl ScriptedRunner {
    fn exiting(exit_code: i32) -> Self {
        Self {
            exit_code,
            ran: Mutex::new(Vec::new()),
        }
    }
}

#[async_trait]
impl CommandRunner for ScriptedRunner {
    fn sandbox(&self) -> SandboxKind {
        SandboxKind::LandlockConfined
    }

    async fn run(&self, request: CommandRequest) -> Result<CommandOutput, CommandError> {
        self.ran.lock().unwrap().push(request);
        Ok(CommandOutput {
            exit_code: Some(self.exit_code),
            stdout: "running 3 tests\n".into(),
            stderr: if self.exit_code == 0 {
                String::new()
            } else {
                "test parser::empty ... FAILED\n".into()
            },
            truncated: false,
            duration: Duration::from_millis(5),
        })
    }
}

fn file_contains(path: &str, text: &str) -> Check {
    Check::FileContains {
        path: WorkspacePath::parse(path).unwrap(),
        contains: text.into(),
    }
}

fn command(run: &str) -> Check {
    Check::Command {
        run: run.into(),
        timeout_secs: Some(30),
    }
}

// --- verify ------------------------------------------------------------------

#[tokio::test]
async fn file_contains_passes_when_the_text_is_there() {
    let files = MemoryFileSystem::with(&[("hello.txt", "well, hi there\n")]);
    let outcomes = verify(
        &[file_contains("hello.txt", "hi there")],
        files.as_ref(),
        None,
    )
    .await;

    assert_eq!(outcomes.len(), 1);
    assert!(outcomes[0].passed, "{outcomes:?}");
    assert_eq!(outcomes[0].kind, "file_contains");
    assert_eq!(outcomes[0].target, "hello.txt");
}

#[tokio::test]
async fn file_contains_fails_on_missing_text_and_on_a_missing_file() {
    let files = MemoryFileSystem::with(&[("hello.txt", "goodbye\n")]);
    let outcomes = verify(
        &[
            file_contains("hello.txt", "hi"),
            file_contains("absent.txt", "hi"),
        ],
        files.as_ref(),
        None,
    )
    .await;

    assert!(!outcomes[0].passed);
    assert!(
        outcomes[0].detail.contains("does not contain"),
        "{outcomes:?}"
    );
    assert!(!outcomes[1].passed);
    assert!(
        outcomes[1].detail.contains("does not exist"),
        "{outcomes:?}"
    );
}

#[tokio::test]
async fn a_command_check_runs_through_the_runner_at_the_workspace_root() {
    let files = MemoryFileSystem::with(&[]);
    let runner = ScriptedRunner::exiting(0);

    let outcomes = verify(&[command("cargo test")], files.as_ref(), Some(&runner)).await;

    assert!(outcomes[0].passed, "{outcomes:?}");
    let ran = runner.ran.lock().unwrap();
    assert_eq!(ran.len(), 1);
    assert_eq!(ran[0].command, "cargo test");
    assert!(ran[0].cwd.is_root(), "checks run at the workspace root");
    assert_eq!(ran[0].timeout, Duration::from_secs(30));
}

#[tokio::test]
async fn a_failing_command_check_keeps_the_end_of_the_output() {
    let files = MemoryFileSystem::with(&[]);
    let runner = ScriptedRunner::exiting(101);

    let outcomes = verify(&[command("cargo test")], files.as_ref(), Some(&runner)).await;

    assert!(!outcomes[0].passed);
    assert!(outcomes[0].detail.starts_with("exit 101"), "{outcomes:?}");
    assert!(outcomes[0].detail.contains("FAILED"), "{outcomes:?}");
}

#[tokio::test]
async fn a_command_check_without_a_runner_fails_rather_than_being_skipped() {
    let files = MemoryFileSystem::with(&[("a", "b")]);
    let outcomes = verify(
        &[command("true"), file_contains("a", "b")],
        files.as_ref(),
        None,
    )
    .await;

    assert!(!outcomes[0].passed, "a skipped check would read as a pass");
    assert!(outcomes[1].passed, "later checks still run");
}

// --- metrics -----------------------------------------------------------------

#[test]
fn the_metrics_sink_counts_what_the_loop_reports() {
    let sink = MetricsSink::new();
    let name = ToolName::new("write_file").unwrap();
    let events = [
        AgentEvent::IterationStarted {
            iteration: 1,
            limit: 5,
        },
        AgentEvent::HistoryCompacted {
            folded_messages: 4,
            before_bytes: 100,
            after_bytes: 10,
        },
        AgentEvent::HistoryTrimmed {
            dropped_messages: 2,
        },
        AgentEvent::HistoryTrimmed {
            dropped_messages: 1,
        },
        AgentEvent::ModelResponded {
            provider: ProviderId::new("p"),
            model: ModelId::new("m"),
            stop_reason: StopReason::ToolUse,
            usage: TokenUsage::default(),
            latency: Duration::from_millis(250),
        },
        AgentEvent::ToolCallFinished {
            name: name.clone(),
            call_id: ToolCallId::new("1"),
            is_error: true,
            summary: String::new(),
            duration: Duration::ZERO,
        },
        AgentEvent::ToolCallFinished {
            name: name.clone(),
            call_id: ToolCallId::new("2"),
            is_error: false,
            summary: String::new(),
            duration: Duration::ZERO,
        },
        AgentEvent::ToolCallDenied {
            name,
            reason: "no".into(),
        },
        AgentEvent::IterationStarted {
            iteration: 2,
            limit: 5,
        },
    ];
    for event in events {
        sink.emit(event);
    }

    let metrics = sink.snapshot();
    assert_eq!(metrics.iterations, 2);
    assert_eq!(metrics.compactions, 1);
    assert_eq!(metrics.trims, 2);
    assert_eq!(metrics.tool_errors, 1);
    assert_eq!(metrics.denials, 1);
    assert_eq!(metrics.model_requests, 1);
    assert_eq!(metrics.model_latency, Duration::from_millis(250));
}

// --- the real loop into the sink ---------------------------------------------

struct Scripted(Mutex<Vec<ChatResponse>>);

#[async_trait]
impl LlmProvider for Scripted {
    fn id(&self) -> ProviderId {
        ProviderId::new("scripted")
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities::default()
    }

    async fn chat(&self, _request: ChatRequest) -> Result<ChatResponse, LlmError> {
        let mut script = self.0.lock().unwrap();
        let message = if script.is_empty() {
            Message::assistant_text("(script exhausted)")
        } else {
            return Ok(script.remove(0));
        };
        Ok(response(message, StopReason::EndTurn))
    }
}

fn response(message: Message, stop_reason: StopReason) -> ChatResponse {
    ChatResponse {
        message,
        stop_reason,
        usage: TokenUsage {
            input_tokens: 10,
            output_tokens: 5,
            ..TokenUsage::default()
        },
        model: ModelId::new("scripted-model"),
        provider: ProviderId::new("scripted"),
    }
}

fn tool_use(id: &str, tool: &str, arguments: serde_json::Value) -> ChatResponse {
    let call = ToolCall::new(ToolCallId::new(id), ToolName::new(tool).unwrap(), arguments);
    response(
        Message::assistant(vec![ContentBlock::ToolCall(call)]),
        StopReason::ToolUse,
    )
}

struct FakeContext;

#[async_trait]
impl ContextProvider for FakeContext {
    async fn snapshot(&self) -> Result<ContextSnapshot, FsError> {
        Ok(ContextSnapshot {
            workspace_root: "/workspace".into(),
            os: "linux".into(),
            today: "2026-11-08".into(),
            is_git_repository: false,
            project_instructions: None,
            nested_instructions: Vec::new(),
            directory_overview: Vec::new(),
        })
    }
}

/// Lets reads through and refuses writes - the shape of a non-interactive
/// gate under `read_only_auto`.
struct DenyWrites;

#[async_trait]
impl ApprovalGate for DenyWrites {
    async fn authorize(
        &self,
        request: &ApprovalRequest,
    ) -> Result<ApprovalDecision, ApprovalError> {
        Ok(if request.call.name.as_str() == "write_file" {
            ApprovalDecision::Deny {
                reason: "not in eval".into(),
            }
        } else {
            ApprovalDecision::Approve
        })
    }
}

#[tokio::test]
async fn a_real_run_leaves_its_tool_errors_and_denials_in_the_sink() {
    let files = MemoryFileSystem::with(&[]);
    let root = Arc::new(WorkspaceRoot::new("/workspace").unwrap());
    let tools = Arc::new(
        ToolRegistry::new()
            .with(Arc::new(ReadFileTool::new(files.clone(), root.clone())))
            .with(Arc::new(WriteFileTool::new(files.clone(), root))),
    );
    let provider = Arc::new(Scripted(Mutex::new(vec![
        tool_use("1", "read_file", json!({"path": "missing.txt"})),
        tool_use(
            "2",
            "write_file",
            json!({"path": "out.txt", "content": "x"}),
        ),
        response(Message::assistant_text("done"), StopReason::EndTurn),
    ])));
    let sink = Arc::new(MetricsSink::new());
    let agent = AgentLoop::new(
        AgentDependencies {
            llm: provider,
            tools,
            approval: Arc::new(DenyWrites),
            events: sink.clone(),
            context: Arc::new(FakeContext),
            prompt: Arc::new(DefaultPromptBuilder),
            log: Arc::new(NullConversationLog),
        },
        AgentLoopConfig::default(),
    );

    let mut session = Session::new("eval-metrics");
    let outcome = agent.run(&mut session, "go").await.unwrap();

    let metrics = sink.snapshot();
    assert_eq!(metrics.iterations, outcome.iterations);
    assert_eq!(metrics.iterations, 3);
    assert_eq!(metrics.model_requests, 3);
    assert_eq!(metrics.tool_calls, 1, "the denied call never started");
    assert_eq!(
        metrics.tool_errors, 1,
        "reading a missing file is a tool error"
    );
    assert_eq!(metrics.denials, 1);
}

/// Fails every approval: the gate itself is broken, as a closed terminal
/// would leave it.
struct BrokenGate;

#[async_trait]
impl ApprovalGate for BrokenGate {
    async fn authorize(&self, _: &ApprovalRequest) -> Result<ApprovalDecision, ApprovalError> {
        Err(ApprovalError::Unavailable("the terminal went away".into()))
    }
}

async fn metrics_of_one_call(approval: Arc<dyn ApprovalGate>, tool: &str) -> RunMetrics {
    let files = MemoryFileSystem::with(&[]);
    let root = Arc::new(WorkspaceRoot::new("/workspace").unwrap());
    let tools = Arc::new(
        ToolRegistry::new().with(Arc::new(ReadFileTool::new(files.clone(), root.clone()))),
    );
    let provider = Arc::new(Scripted(Mutex::new(vec![
        tool_use("1", tool, json!({"path": "a.txt"})),
        response(Message::assistant_text("done"), StopReason::EndTurn),
    ])));
    let sink = Arc::new(MetricsSink::new());
    let agent = AgentLoop::new(
        AgentDependencies {
            llm: provider,
            tools,
            approval,
            events: sink.clone(),
            context: Arc::new(FakeContext),
            prompt: Arc::new(DefaultPromptBuilder),
            log: Arc::new(NullConversationLog),
        },
        AgentLoopConfig::default(),
    );
    let mut session = Session::new("eval-metrics");
    agent.run(&mut session, "go").await.unwrap();
    sink.snapshot()
}

#[tokio::test]
async fn a_call_to_an_unknown_tool_counts_as_a_failed_tool_call() {
    let metrics = metrics_of_one_call(Arc::new(DenyWrites), "no_such_tool").await;
    assert_eq!(metrics.tool_calls, 1, "{metrics:?}");
    assert_eq!(metrics.tool_errors, 1, "{metrics:?}");
    assert_eq!(metrics.denials, 0, "nobody refused it: {metrics:?}");
}

#[tokio::test]
async fn a_call_whose_approval_could_not_be_obtained_counts_as_a_failed_tool_call() {
    let metrics = metrics_of_one_call(Arc::new(BrokenGate), "read_file").await;
    assert_eq!(metrics.tool_calls, 1, "{metrics:?}");
    assert_eq!(metrics.tool_errors, 1, "{metrics:?}");
}
