//! Do the operator's hooks run, refuse, observe and stay confined as
//! promised?
//!
//! Every hook here is a real `/bin/sh` line under the real sandbox, and every
//! claim is checked against what the process did - a file it wrote, a file it
//! could not write - not against a fake runner.
//!
//! Linux only: elsewhere there is no sandbox, and starting confined hooks
//! fails (which `the_default_sandbox_either_confines_or_stops_startup`-style
//! checks in the composition tests cover).

#![cfg(target_os = "linux")]

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use agent_domain::error::{ApprovalError, ToolError};
use agent_domain::model::dependency::{DependencyChangeDescription, ManifestChange, ManifestKind};
use agent_domain::model::tool::{
    Before, PlannedWrite, ToolCall, ToolCallId, ToolDefinition, ToolName, ToolOutcome, ToolSafety,
};
use agent_domain::model::workspace::WorkspacePath;
use agent_domain::ports::approval::{
    ApprovalDecision, ApprovalGate, ApprovalRequest, DependencyApprovalRequest,
};
use agent_domain::ports::command::SandboxKind;
use agent_domain::ports::tool::Tool;
use agent_infrastructure::exec::{SandboxRequirement, detect_sandbox};
use agent_infrastructure::hooks::{HookGate, HookedTool, Hooks, HooksConfig};
use agent_infrastructure::secrets::Redactor;
use async_trait::async_trait;
use serde_json::{Value, json};

/// Long enough for the redactor to take it as a secret.
const KEY: &str = "sk-ant-api03-0123456789abcdef";

struct Fixture {
    _dirs: Vec<tempfile::TempDir>,
    workspace: PathBuf,
    outside: PathBuf,
}

fn canonical_tempdir() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = std::fs::canonicalize(dir.path()).unwrap();
    (dir, path)
}

fn fixture() -> Fixture {
    // The dev container is the reference environment: missing Landlock is a
    // loud failure, not a silent skip.
    let available = detect_sandbox();
    assert_eq!(
        available.kind,
        SandboxKind::LandlockConfined,
        "shortfall: {:?}",
        available.shortfall
    );
    let (workspace_dir, workspace) = canonical_tempdir();
    let (outside_dir, outside) = canonical_tempdir();
    Fixture {
        _dirs: vec![workspace_dir, outside_dir],
        workspace,
        outside,
    }
}

impl Fixture {
    fn hooks(&self, config: Value, requirement: SandboxRequirement) -> Arc<Hooks> {
        let config = HooksConfig::parse(&config.to_string()).expect("a valid hooks file");
        let hooks = Hooks::start(
            config,
            requirement,
            &self.workspace,
            Vec::new(),
            Arc::new(Redactor::new([KEY.to_string()])),
        )
        .expect("hooks start");
        hooks.begin_turn("session-1");
        Arc::new(hooks)
    }

    fn confined(&self, config: Value) -> Arc<Hooks> {
        self.hooks(config, SandboxRequirement::Confined)
    }

    fn read_json(&self, name: &str) -> Value {
        let path = self.workspace.join(name);
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("the hook did not write `{name}`: {error}"));
        serde_json::from_str(&text).unwrap_or_else(|error| panic!("{text:?}: {error}"))
    }

    fn ran(&self, marker: &str) -> bool {
        self.workspace.join(marker).exists()
    }
}

/// Answers with `decision` and counts what it was asked.
struct Inner {
    decision: ApprovalDecision,
    asked: Mutex<Vec<String>>,
    dependency_questions: Mutex<usize>,
}

impl Inner {
    fn answering(decision: ApprovalDecision) -> Arc<Self> {
        Arc::new(Self {
            decision,
            asked: Mutex::new(Vec::new()),
            dependency_questions: Mutex::new(0),
        })
    }

    fn approving() -> Arc<Self> {
        Self::answering(ApprovalDecision::Approve)
    }

    fn asked(&self) -> Vec<String> {
        self.asked.lock().unwrap().clone()
    }
}

#[async_trait]
impl ApprovalGate for Inner {
    async fn authorize(
        &self,
        request: &ApprovalRequest,
    ) -> Result<ApprovalDecision, ApprovalError> {
        self.asked
            .lock()
            .unwrap()
            .push(request.call.name.to_string());
        Ok(self.decision.clone())
    }

    async fn authorize_dependency_change(
        &self,
        _request: &DependencyApprovalRequest,
    ) -> Result<ApprovalDecision, ApprovalError> {
        *self.dependency_questions.lock().unwrap() += 1;
        Ok(self.decision.clone())
    }

    async fn reauthorize_write(
        &self,
        request: &ApprovalRequest,
        _writes: &[PlannedWrite],
    ) -> Result<ApprovalDecision, ApprovalError> {
        self.asked
            .lock()
            .unwrap()
            .push(format!("again: {}", request.call.name));
        Ok(self.decision.clone())
    }
}

fn call(id: &str, tool: &str, arguments: Value) -> ToolCall {
    ToolCall::new(ToolCallId::new(id), ToolName::new(tool).unwrap(), arguments)
}

fn request(call: ToolCall, safety: ToolSafety) -> ApprovalRequest {
    ApprovalRequest {
        summary: call.name.to_string(),
        call,
        safety,
        must_ask: false,
    }
}

fn refusal(decision: ApprovalDecision) -> String {
    match decision {
        ApprovalDecision::Deny { reason } => reason,
        ApprovalDecision::Approve => panic!("the call must be refused"),
    }
}

// --- pre_tool_use ------------------------------------------------------------

#[tokio::test]
async fn a_refusing_pre_hook_denies_with_its_stderr_and_nobody_else_is_asked() {
    let fixture = fixture();
    let hooks = fixture.confined(json!({"pre_tool_use": [{
        "match": "*",
        "command": "echo 'no writes to migrations/' >&2; echo 'second line' >&2; exit 1",
    }]}));
    let inner = Inner::approving();
    let gate = HookGate::new(inner.clone(), hooks);

    let reason = refusal(
        gate.authorize(&request(
            call("c1", "write_file", json!({"path": "migrations/1.sql"})),
            ToolSafety::Mutating,
        ))
        .await
        .unwrap(),
    );

    assert_eq!(reason, "no writes to migrations/ second line");
    assert!(
        inner.asked().is_empty(),
        "a refused call must not reach the rules or the human: {:?}",
        inner.asked()
    );
}

#[tokio::test]
async fn a_pre_hook_that_runs_out_of_time_refuses() {
    let fixture = fixture();
    let hooks = fixture.confined(json!({"pre_tool_use": [{
        "match": "*", "command": "sleep 30", "timeout_secs": 1,
    }]}));
    let inner = Inner::approving();
    let gate = HookGate::new(inner.clone(), hooks);

    let started = Instant::now();
    let reason = refusal(
        gate.authorize(&request(
            call("c1", "read_file", json!({})),
            ToolSafety::ReadOnly,
        ))
        .await
        .unwrap(),
    );

    assert!(reason.contains("did not finish within 1s"), "{reason}");
    assert!(
        started.elapsed() < Duration::from_secs(20),
        "the timeout must cut the hook off, took {:?}",
        started.elapsed()
    );
    assert!(inner.asked().is_empty());
}

#[tokio::test]
async fn a_hook_that_cannot_start_refuses_rather_than_passing() {
    let fixture = fixture();
    // `exec` of a program that does not exist: the shell exits 127.
    let hooks = fixture.confined(json!({"pre_tool_use": [{
        "match": "*", "command": "exec /nonexistent/check",
    }]}));
    let gate = HookGate::new(Inner::approving(), hooks);

    let decision = gate
        .authorize(&request(
            call("c1", "read_file", json!({})),
            ToolSafety::ReadOnly,
        ))
        .await
        .unwrap();
    assert!(!decision.is_approved(), "{decision:?}");
}

#[tokio::test]
async fn a_passing_pre_hook_hands_the_question_on_and_approves_nothing_itself() {
    let fixture = fixture();
    let hooks = fixture.confined(json!({"pre_tool_use": [{
        "match": "*", "command": "touch passed; exit 0",
    }]}));
    let inner = Inner::answering(ApprovalDecision::Deny {
        reason: "the human said no".into(),
    });
    let gate = HookGate::new(inner.clone(), hooks);

    let reason = refusal(
        gate.authorize(&request(
            call("c1", "run_command", json!({"command": "ls"})),
            ToolSafety::Destructive,
        ))
        .await
        .unwrap(),
    );

    assert!(fixture.ran("passed"), "the hook must have run");
    assert_eq!(reason, "the human said no");
    assert_eq!(inner.asked(), ["run_command"]);
}

#[tokio::test]
async fn only_hooks_whose_match_names_the_tool_run() {
    let fixture = fixture();
    let hooks = fixture.confined(json!({"pre_tool_use": [
        {"match": "run_command", "command": "touch for-commands; exit 1"},
        {"match": "write_file", "command": "touch for-writes; exit 0"},
    ]}));
    let inner = Inner::approving();
    let gate = HookGate::new(inner.clone(), hooks);

    let decision = gate
        .authorize(&request(
            call("c1", "write_file", json!({"path": "a"})),
            ToolSafety::Mutating,
        ))
        .await
        .unwrap();

    assert_eq!(decision, ApprovalDecision::Approve);
    assert!(fixture.ran("for-writes"));
    assert!(
        !fixture.ran("for-commands"),
        "a hook on another tool must not run"
    );
}

#[tokio::test]
async fn the_second_look_at_a_write_goes_to_the_inner_gate_without_running_hooks_again() {
    let fixture = fixture();
    let hooks = fixture.confined(json!({"pre_tool_use": [{
        "match": "*", "command": "touch asked; exit 1",
    }]}));
    let inner = Inner::answering(ApprovalDecision::Deny {
        reason: "looked again".into(),
    });
    let gate = HookGate::new(inner.clone(), hooks);

    let decision = gate
        .reauthorize_write(
            &request(
                call("c1", "write_file", json!({"path": "a"})),
                ToolSafety::Mutating,
            ),
            &[],
        )
        .await
        .unwrap();

    assert_eq!(
        decision,
        ApprovalDecision::Deny {
            reason: "looked again".into()
        },
        "the inner gate's answer, not the trait's default"
    );
    assert_eq!(inner.asked(), ["again: write_file"]);
    assert!(!fixture.ran("asked"), "the hooks judged the call already");
}

#[tokio::test]
async fn the_dependency_question_goes_straight_to_the_inner_gate() {
    let fixture = fixture();
    let hooks = fixture.confined(json!({"pre_tool_use": [{
        "match": "*", "command": "touch asked; exit 1",
    }]}));
    let inner = Inner::approving();
    let gate = HookGate::new(inner.clone(), hooks);

    let decision = gate
        .authorize_dependency_change(&DependencyApprovalRequest {
            call: call("c1", "write_file", json!({"path": "Cargo.toml"})),
            safety: ToolSafety::Mutating,
            changes: vec![ManifestChange {
                path: WorkspacePath::parse("Cargo.toml").unwrap(),
                target: None,
                kind: ManifestKind::CargoToml,
                before: Before::Absent,
                description: DependencyChangeDescription::Lines {
                    reason: "new file".into(),
                    added: vec!["[dependencies]".into()],
                    removed: Vec::new(),
                    omitted: 0,
                },
            }],
        })
        .await
        .unwrap();

    assert_eq!(decision, ApprovalDecision::Approve);
    assert_eq!(*inner.dependency_questions.lock().unwrap(), 1);
    assert!(
        !fixture.ran("asked"),
        "no hook answers the dependency question"
    );
}

#[tokio::test]
async fn a_pre_hook_reads_the_call_as_documented() {
    let fixture = fixture();
    let hooks = fixture.confined(json!({"pre_tool_use": [{
        "match": "run_command", "command": "cat > pre.json",
    }]}));
    let gate = HookGate::new(Inner::approving(), hooks);

    gate.authorize(&request(
        call(
            "call-7",
            "run_command",
            json!({"command": "cargo test", "cwd": "."}),
        ),
        ToolSafety::Destructive,
    ))
    .await
    .unwrap();

    assert_eq!(
        fixture.read_json("pre.json"),
        json!({
            "v": 1,
            "event": "pre_tool_use",
            "session_id": "session-1",
            "tool": "run_command",
            "safety": "destructive",
            "call_id": "call-7",
            "arguments": {"command": "cargo test", "cwd": "."},
        })
    );
}

// --- post_tool_use -----------------------------------------------------------

/// Returns its `text` argument, as output or as an error.
struct Echo;

#[async_trait]
impl Tool for Echo {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: ToolName::new("echo").unwrap(),
            description: "echoes".into(),
            input_schema: json!({"type": "object"}),
            safety: ToolSafety::ReadOnly,
        }
    }

    async fn preview(&self, _arguments: &Value) -> Result<Vec<PlannedWrite>, ToolError> {
        Ok(Vec::new())
    }

    async fn execute(&self, arguments: Value) -> Result<ToolOutcome, ToolError> {
        let text = arguments["text"].as_str().unwrap_or_default().to_string();
        if arguments["fail"].as_bool() == Some(true) {
            return Err(ToolError::execution(&self.definition().name, text));
        }
        Ok(ToolOutcome::new(text).with_summary("echoed"))
    }
}

#[tokio::test]
async fn a_post_hook_sees_the_redacted_output_and_changes_nothing() {
    let fixture = fixture();
    let hooks = fixture.confined(json!({"post_tool_use": [{
        "match": "echo",
        // Fails loudly on purpose: a failing observer must not touch the result.
        "command": "cat > post.json; echo 'formatter broke' >&2; exit 3",
    }]}));
    let gate = HookGate::new(Inner::approving(), hooks.clone());
    let tool = HookedTool::wrap(Arc::new(Echo), &hooks);

    let arguments = json!({"text": format!("AGENT_API_KEY={KEY}\n")});
    // The dispatcher's order: approved first, then run.
    gate.authorize(&request(
        call("call-9", "echo", arguments.clone()),
        ToolSafety::ReadOnly,
    ))
    .await
    .unwrap();
    let outcome = tool.execute(arguments.clone()).await.unwrap();

    let unwrapped = Echo.execute(arguments.clone()).await.unwrap();
    assert_eq!(
        outcome, unwrapped,
        "the model must get the tool's own result"
    );

    assert_eq!(
        fixture.read_json("post.json"),
        json!({
            "v": 1,
            "event": "post_tool_use",
            "session_id": "session-1",
            "tool": "echo",
            "safety": "read_only",
            "call_id": "call-9",
            "arguments": arguments,
            "is_error": false,
            "output": "AGENT_API_KEY=[REDACTED]\n",
        })
    );
}

#[tokio::test]
async fn a_post_hook_is_told_about_a_failed_call_and_the_error_stands() {
    let fixture = fixture();
    let hooks = fixture.confined(json!({"post_tool_use": [{
        "match": "*", "command": "cat > post.json",
    }]}));
    let tool = HookedTool::wrap(Arc::new(Echo), &hooks);

    let arguments = json!({"text": "disk full", "fail": true});
    let error = tool.execute(arguments.clone()).await.unwrap_err();
    assert_eq!(
        error.to_string(),
        Echo.execute(arguments).await.unwrap_err().to_string(),
        "the error must reach the model unchanged"
    );

    let seen = fixture.read_json("post.json");
    assert_eq!(seen["is_error"], json!(true));
    assert_eq!(seen["output"], json!(error.to_string()));
    assert_eq!(
        seen["call_id"],
        Value::Null,
        "never approved through the gate, so no call id is known"
    );
}

#[tokio::test]
async fn a_tool_no_post_hook_is_about_is_not_wrapped() {
    let fixture = fixture();
    let hooks = fixture.confined(json!({"post_tool_use": [{
        "match": "write_file", "command": "exit 0",
    }]}));
    let inner: Arc<dyn Tool> = Arc::new(Echo);
    let wrapped = HookedTool::wrap(inner.clone(), &hooks);
    assert!(Arc::ptr_eq(&inner, &wrapped));
}

// --- stop --------------------------------------------------------------------

#[tokio::test]
async fn stop_hooks_run_in_order_with_the_reason() {
    let fixture = fixture();
    let hooks = fixture.confined(json!({"stop": [
        {"command": "cat > stop.json"},
        {"command": "exit 9"},
        {"command": "touch after-a-failure"},
    ]}));

    hooks.stop("session-2", "completed").await;

    assert_eq!(
        fixture.read_json("stop.json"),
        json!({"v": 1, "event": "stop", "session_id": "session-2", "reason": "completed"})
    );
    assert!(
        fixture.ran("after-a-failure"),
        "a failing stop hook must not keep the next from running"
    );
}

// --- confinement -------------------------------------------------------------

/// A stop hook that tries to write in the workspace and outside it.
fn writer(outside: &Path) -> Value {
    json!({"stop": [{
        "command": format!(
            "echo ok > inside; echo pwned > '{}/escape'",
            outside.display()
        ),
    }]})
}

#[tokio::test]
async fn a_hook_writes_the_workspace_and_nothing_outside_it() {
    let fixture = fixture();

    // Control: unconfined, the same line does write outside, so the refusal
    // below is the kernel's and not a broken command.
    fixture
        .hooks(writer(&fixture.outside), SandboxRequirement::Disabled)
        .stop("s", "completed")
        .await;
    assert!(
        fixture.outside.join("escape").exists(),
        "the control must write"
    );
    std::fs::remove_file(fixture.outside.join("escape")).unwrap();
    std::fs::remove_file(fixture.workspace.join("inside")).unwrap();

    let hooks = fixture.confined(writer(&fixture.outside));
    hooks.stop("s", "completed").await;

    assert!(
        fixture.ran("inside"),
        "the workspace must stay writable, for formatters"
    );
    assert!(
        !fixture.outside.join("escape").exists(),
        "a confined hook must not write outside the workspace"
    );
}
