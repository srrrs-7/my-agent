//! The dependency gate: a change to a dependency file is approved on its own,
//! right before it is made, and no earlier answer approves it.

mod common;

use std::sync::{Arc, Mutex};

use agent_application::agent::Interrupt;
use agent_application::tools::ToolRegistry;
use agent_application::tools::file::{EditFileTool, WriteFileTool};
use agent_domain::error::{ApprovalError, ToolError};
use agent_domain::model::dependency::{DependencyChangeDescription, ManifestKind};
use agent_domain::model::tool::{ToolDefinition, ToolName, ToolOutcome, ToolSafety};
use agent_domain::ports::approval::{
    ApprovalDecision, ApprovalGate, ApprovalRequest, DependencyApprovalRequest,
};
use agent_domain::ports::events::{AgentEvent, NullEventSink};
use agent_domain::ports::tool::Tool;
use async_trait::async_trait;
use serde_json::json;

use common::*;

/// Approves every call, and answers the dependency question with `decision`,
/// recording what it was shown.
struct DependencyGate {
    decision: ApprovalDecision,
    asked: Mutex<Vec<DependencyApprovalRequest>>,
}

impl DependencyGate {
    fn answering(decision: ApprovalDecision) -> Arc<Self> {
        Arc::new(Self {
            decision,
            asked: Mutex::new(Vec::new()),
        })
    }

    fn asked(&self) -> Vec<DependencyApprovalRequest> {
        self.asked.lock().unwrap().clone()
    }
}

#[async_trait]
impl ApprovalGate for DependencyGate {
    async fn authorize(&self, _: &ApprovalRequest) -> Result<ApprovalDecision, ApprovalError> {
        Ok(ApprovalDecision::Approve)
    }

    async fn authorize_dependency_change(
        &self,
        request: &DependencyApprovalRequest,
    ) -> Result<ApprovalDecision, ApprovalError> {
        self.asked.lock().unwrap().push(request.clone());
        Ok(self.decision.clone())
    }
}

/// Approves every call, then is interrupted while the dependency prompt is up.
struct InterruptedDependencyPrompt {
    interrupt: Interrupt,
}

#[async_trait]
impl ApprovalGate for InterruptedDependencyPrompt {
    async fn authorize(&self, _: &ApprovalRequest) -> Result<ApprovalDecision, ApprovalError> {
        Ok(ApprovalDecision::Approve)
    }

    async fn authorize_dependency_change(
        &self,
        _: &DependencyApprovalRequest,
    ) -> Result<ApprovalDecision, ApprovalError> {
        self.interrupt.fire();
        std::future::pending().await
    }
}

const MANIFEST: &str = "[package]\nname = \"demo\"\n\n[dependencies]\nserde = \"1\"\n";

fn file_registry(file_system: Arc<MemoryFileSystem>) -> Arc<ToolRegistry> {
    Arc::new(
        ToolRegistry::new()
            .with(Arc::new(WriteFileTool::new(file_system.clone(), root())))
            .with(Arc::new(EditFileTool::new(file_system, root()))),
    )
}

fn add_tokio() -> serde_json::Value {
    json!({
        "path": "Cargo.toml",
        "old_string": "serde = \"1\"\n",
        "new_string": "serde = \"1\"\ntokio = \"1\"\n",
    })
}

fn denied() -> ApprovalDecision {
    ApprovalDecision::Deny {
        reason: "the user answered no".into(),
    }
}

fn shown(request: &DependencyApprovalRequest) -> Vec<String> {
    request
        .changes
        .iter()
        .flat_map(|change| match &change.description {
            DependencyChangeDescription::Dependencies(changes) => {
                changes.iter().map(ToString::to_string).collect::<Vec<_>>()
            }
            other => panic!("expected parsed dependencies, got {other:?}"),
        })
        .collect()
}

#[tokio::test]
async fn approving_every_call_does_not_approve_a_dependency_change() {
    let file_system = MemoryFileSystem::with(&[("Cargo.toml", MANIFEST)]);
    let gate = DependencyGate::answering(denied());
    let events = Arc::new(RecordingSink::default());
    let batch = calls(&[("e1", "edit_file", add_tokio())]);

    let results = dispatcher(
        file_registry(file_system.clone()),
        gate.clone(),
        events.clone(),
    )
    .dispatch(&batch, &Interrupt::new())
    .await;

    let asked = gate.asked();
    assert_eq!(asked.len(), 1, "the dependency question was asked");
    assert_eq!(asked[0].changes[0].kind, ManifestKind::CargoToml);
    assert_eq!(shown(&asked[0]), ["+ tokio \"1\""]);

    assert!(results[0].is_error, "{results:?}");
    assert!(
        results[0].content.contains("Cargo.toml") && results[0].content.contains("Do not retry"),
        "the model is told what was refused and what to do instead: {}",
        results[0].content
    );
    assert_eq!(
        file_system.get("Cargo.toml").as_deref(),
        Some(MANIFEST),
        "a refused change is not written"
    );
    let events = events.events.lock().unwrap();
    assert!(
        events
            .iter()
            .any(|event| matches!(event, AgentEvent::ToolCallDenied { .. })),
        "{events:?}"
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, AgentEvent::ToolCallStarted { .. })),
        "a refused call never starts: {events:?}"
    );
}

#[tokio::test]
async fn a_gate_that_knows_nothing_of_dependencies_refuses_them() {
    // `AlwaysApprove` only implements `authorize`. The port's default must
    // read as "no", or every older gate would wave dependencies through.
    let file_system = MemoryFileSystem::with(&[("Cargo.toml", MANIFEST)]);
    let batch = calls(&[("e1", "edit_file", add_tokio())]);

    let results = dispatcher(
        file_registry(file_system.clone()),
        Arc::new(AlwaysApprove),
        Arc::new(NullEventSink),
    )
    .dispatch(&batch, &Interrupt::new())
    .await;

    assert!(results[0].is_error, "{results:?}");
    assert_eq!(file_system.get("Cargo.toml").as_deref(), Some(MANIFEST));
}

#[tokio::test]
async fn an_approved_dependency_change_is_written() {
    let file_system = MemoryFileSystem::with(&[("Cargo.toml", MANIFEST)]);
    let gate = DependencyGate::answering(ApprovalDecision::Approve);
    let batch = calls(&[("e1", "edit_file", add_tokio())]);

    let results = dispatcher(
        file_registry(file_system.clone()),
        gate,
        Arc::new(NullEventSink),
    )
    .dispatch(&batch, &Interrupt::new())
    .await;

    assert!(!results[0].is_error, "{results:?}");
    assert!(file_system.get("Cargo.toml").unwrap().contains("tokio"));
}

#[tokio::test]
async fn the_change_shown_is_against_the_file_as_earlier_calls_left_it() {
    // Two writes to one manifest in one turn. Worked out up front, the second
    // would be shown adding serde again; worked out right before it runs, it
    // adds only tokio - which is what it does.
    let file_system = MemoryFileSystem::with(&[]);
    let gate = DependencyGate::answering(ApprovalDecision::Approve);
    let batch = calls(&[
        (
            "w1",
            "write_file",
            json!({"path": "Cargo.toml", "content": "[dependencies]\nserde = \"1\"\n"}),
        ),
        (
            "w2",
            "write_file",
            json!({"path": "Cargo.toml", "content": "[dependencies]\nserde = \"1\"\ntokio = \"1\"\n"}),
        ),
    ]);

    dispatcher(
        file_registry(file_system),
        gate.clone(),
        Arc::new(NullEventSink),
    )
    .dispatch(&batch, &Interrupt::new())
    .await;

    let asked = gate.asked();
    assert_eq!(asked.len(), 2, "{asked:?}");
    assert_eq!(
        asked[0].changes[0].before,
        agent_domain::model::tool::Before::Absent,
        "the first write creates the file"
    );
    assert_eq!(shown(&asked[0]), ["+ serde \"1\""]);
    assert!(matches!(
        asked[1].changes[0].before,
        agent_domain::model::tool::Before::Text(_)
    ));
    assert_eq!(shown(&asked[1]), ["+ tokio \"1\""]);
}

#[tokio::test]
async fn files_that_declare_no_dependencies_and_unchanged_manifests_are_not_asked_about() {
    let file_system = MemoryFileSystem::with(&[("Cargo.toml", MANIFEST)]);
    let gate = DependencyGate::answering(denied());
    let batch = calls(&[
        (
            "w1",
            "write_file",
            json!({"path": "src/main.rs", "content": "fn main() {}\n"}),
        ),
        (
            "w2",
            "write_file",
            json!({"path": "Cargo.toml", "content": MANIFEST}),
        ),
    ]);

    let results = dispatcher(
        file_registry(file_system),
        gate.clone(),
        Arc::new(NullEventSink),
    )
    .dispatch(&batch, &Interrupt::new())
    .await;

    assert!(results.iter().all(|result| !result.is_error), "{results:?}");
    assert!(gate.asked().is_empty(), "{:?}", gate.asked());
}

#[tokio::test]
async fn a_manifest_spelled_in_another_case_is_still_asked_about() {
    let file_system = MemoryFileSystem::with(&[]);
    let gate = DependencyGate::answering(denied());
    let batch = calls(&[(
        "w1",
        "write_file",
        json!({"path": "crates/x/cargo.TOML", "content": "[dependencies]\nevil = \"1\"\n"}),
    )]);

    let results = dispatcher(
        file_registry(file_system.clone()),
        gate.clone(),
        Arc::new(NullEventSink),
    )
    .dispatch(&batch, &Interrupt::new())
    .await;

    assert_eq!(gate.asked().len(), 1);
    assert!(results[0].is_error);
    assert_eq!(file_system.get("crates/x/cargo.TOML"), None);
}

#[tokio::test]
async fn an_unparseable_manifest_is_still_asked_about() {
    let file_system = MemoryFileSystem::with(&[("package.json", "{\"dependencies\": {}}")]);
    let gate = DependencyGate::answering(denied());
    let batch = calls(&[(
        "w1",
        "write_file",
        json!({"path": "package.json", "content": "{\"dependencies\": {\"evil\": \"1\""}),
    )]);

    dispatcher(
        file_registry(file_system),
        gate.clone(),
        Arc::new(NullEventSink),
    )
    .dispatch(&batch, &Interrupt::new())
    .await;

    let asked = gate.asked();
    assert_eq!(asked.len(), 1, "a parse failure must not skip the question");
    assert!(
        matches!(
            asked[0].changes[0].description,
            DependencyChangeDescription::Lines { .. }
        ),
        "{asked:?}"
    );
}

#[tokio::test]
async fn an_interrupt_at_the_dependency_prompt_runs_nothing() {
    let file_system = MemoryFileSystem::with(&[("Cargo.toml", MANIFEST)]);
    let interrupt = Interrupt::new();
    let events = Arc::new(RecordingSink::default());
    let batch = calls(&[
        ("e1", "edit_file", add_tokio()),
        (
            "w2",
            "write_file",
            json!({"path": "after.txt", "content": "x"}),
        ),
    ]);

    let results = dispatcher(
        file_registry(file_system.clone()),
        Arc::new(InterruptedDependencyPrompt {
            interrupt: interrupt.clone(),
        }),
        events.clone(),
    )
    .dispatch(&batch, &interrupt)
    .await;

    assert_eq!(results.len(), 2);
    for result in &results {
        assert!(
            result.is_error && result.content.contains(NOT_STARTED),
            "{result:?}"
        );
    }
    assert_eq!(file_system.get("Cargo.toml").as_deref(), Some(MANIFEST));
    assert_eq!(file_system.get("after.txt"), None);
    let events = events.events.lock().unwrap();
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, AgentEvent::ToolCallStarted { .. })),
        "{events:?}"
    );
}

#[tokio::test]
async fn a_call_whose_effect_cannot_be_worked_out_is_not_run_but_reports_the_real_error() {
    // The edit cannot apply, so there is nothing to approve. It is not run -
    // its preview failed for a reason other than invalid arguments - but the
    // model still reads why, and nothing changes.
    let file_system = MemoryFileSystem::with(&[("Cargo.toml", MANIFEST)]);
    let gate = DependencyGate::answering(ApprovalDecision::Approve);
    let batch = calls(&[(
        "e1",
        "edit_file",
        json!({"path": "Cargo.toml", "old_string": "not in the file", "new_string": "x"}),
    )]);

    let results = dispatcher(
        file_registry(file_system.clone()),
        gate.clone(),
        Arc::new(NullEventSink),
    )
    .dispatch(&batch, &Interrupt::new())
    .await;

    assert!(gate.asked().is_empty());
    assert!(results[0].is_error);
    assert!(
        results[0]
            .content
            .starts_with("tool `edit_file` failed: `old_string` was not found"),
        "the model reads what it read before: {}",
        results[0].content
    );
    assert_eq!(file_system.get("Cargo.toml").as_deref(), Some(MANIFEST));
}

/// The error a tool's `preview` fails with.
type PreviewFailure = fn(&ToolName) -> ToolError;

/// A writing tool whose `preview` fails with a chosen error, and that counts
/// how often it actually ran.
struct UnpreviewableTool {
    error: PreviewFailure,
    executed: std::sync::atomic::AtomicUsize,
}

impl UnpreviewableTool {
    fn name() -> ToolName {
        ToolName::new("write_somewhere").unwrap()
    }
}

#[async_trait]
impl Tool for UnpreviewableTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: Self::name(),
            description: "writes a file".into(),
            input_schema: json!({"type": "object"}),
            safety: ToolSafety::Mutating,
        }
    }

    async fn preview(
        &self,
        _arguments: &serde_json::Value,
    ) -> Result<Vec<agent_domain::model::tool::PlannedWrite>, ToolError> {
        Err((self.error)(&Self::name()))
    }

    async fn execute(&self, _arguments: serde_json::Value) -> Result<ToolOutcome, ToolError> {
        self.executed
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(ToolOutcome::new("wrote it"))
    }
}

fn unpreviewable(error: PreviewFailure) -> Arc<UnpreviewableTool> {
    Arc::new(UnpreviewableTool {
        error,
        executed: std::sync::atomic::AtomicUsize::new(0),
    })
}

/// A preview that timed out, or hit an I/O error that may not recur, says
/// nothing about what the call writes. Running it anyway would be running it
/// past the dependency gate unchecked.
#[tokio::test]
async fn a_call_whose_writes_cannot_be_worked_out_is_not_run() {
    let failures: [(&str, PreviewFailure); 2] = [
        ("timeout", |tool| ToolError::Timeout {
            tool: tool.clone(),
            seconds: 30,
        }),
        ("execution", |tool| {
            ToolError::execution(tool, "io error on `Cargo.toml`: interrupted")
        }),
    ];
    for (label, error) in failures {
        let tool = unpreviewable(error);
        let gate = DependencyGate::answering(ApprovalDecision::Approve);
        let events = Arc::new(RecordingSink::default());
        let batch = calls(&[("w1", "write_somewhere", json!({}))]);

        let results = dispatcher(
            Arc::new(ToolRegistry::new().with(tool.clone())),
            gate.clone(),
            events.clone(),
        )
        .dispatch(&batch, &Interrupt::new())
        .await;

        assert_eq!(
            tool.executed.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "{label}: the call ran"
        );
        assert!(results[0].is_error, "{label}: {results:?}");
        let original = error(&UnpreviewableTool::name()).to_string();
        assert!(
            results[0].content.starts_with(&original)
                && results[0].content.ends_with(
                    "(Nothing was written; the change could not be checked against the \
                     dependency gate.)"
                ),
            "{label}: the tool's own error first, then the note: {}",
            results[0].content
        );
        assert!(gate.asked().is_empty(), "{label}: nothing to ask about");
        let events = events.events.lock().unwrap();
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, AgentEvent::ToolCallDenied { .. })),
            "{label}: a failed preview is not a denial: {events:?}"
        );
        assert!(
            events
                .iter()
                .any(|event| matches!(event, AgentEvent::ToolCallFinished { is_error: true, .. })),
            "{label}: shown as an ordinary failed call: {events:?}"
        );
    }
}

/// Arguments `execute` rejects before writing anything: running it is safe,
/// and gives the model the real error rather than a second-hand one.
#[tokio::test]
async fn a_call_with_invalid_arguments_still_reaches_the_tool() {
    let tool = unpreviewable(|tool| ToolError::invalid_input(tool, "`path` is required"));
    let batch = calls(&[("w1", "write_somewhere", json!({}))]);

    dispatcher(
        Arc::new(ToolRegistry::new().with(tool.clone())),
        DependencyGate::answering(ApprovalDecision::Approve),
        Arc::new(NullEventSink),
    )
    .dispatch(&batch, &Interrupt::new())
    .await;

    assert_eq!(tool.executed.load(std::sync::atomic::Ordering::SeqCst), 1);
}
