//! The operator's path rules, judged again right before a write is made.
//!
//! The first approval of a turn is given for every call before any of them
//! runs. An earlier call can then turn the path a later write names into a
//! link to somewhere else, so the dispatcher asks the gates again, with the
//! writes worked out at that moment. These tests change the filesystem
//! between the two looks the way a command would: by a call earlier in the
//! same batch.

mod common;

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use agent_application::agent::Interrupt;
use agent_application::approval::RuleGate;
use agent_application::tools::ToolRegistry;
use agent_application::tools::file::WriteFileTool;
use agent_domain::error::{ApprovalError, FsError, ToolError};
use agent_domain::model::access::PathPattern;
use agent_domain::model::permission::{Effect, PermissionRule, PermissionRules, SubjectPattern};
use agent_domain::model::tool::{
    PlannedWrite, ToolDefinition, ToolName, ToolOutcome, ToolResult, ToolSafety,
};
use agent_domain::model::workspace::WorkspacePath;
use agent_domain::ports::approval::{
    ApprovalDecision, ApprovalGate, ApprovalRequest, DependencyApprovalRequest,
};
use agent_domain::ports::events::AgentEvent;
use agent_domain::ports::file_system::{DirEntry, FileSystem};
use agent_domain::ports::tool::Tool;
use async_trait::async_trait;
use serde_json::{Value, json};

use common::*;

/// Memory files plus symbolic links, which every operation follows - as a
/// real filesystem does.
struct LinkingFileSystem {
    files: Arc<MemoryFileSystem>,
    links: Mutex<HashMap<String, String>>,
}

impl LinkingFileSystem {
    fn with(files: &[(&str, &str)]) -> Arc<Self> {
        Arc::new(Self {
            files: MemoryFileSystem::with(files),
            links: Mutex::default(),
        })
    }

    fn follow(&self, path: &WorkspacePath) -> WorkspacePath {
        match self.links.lock().unwrap().get(&path.display()) {
            Some(target) => WorkspacePath::parse(target).unwrap(),
            None => path.clone(),
        }
    }

    fn get(&self, path: &str) -> Option<String> {
        self.files.get(path)
    }
}

#[async_trait]
impl FileSystem for LinkingFileSystem {
    async fn read_to_string(&self, path: &WorkspacePath) -> Result<String, FsError> {
        self.files.read_to_string(&self.follow(path)).await
    }

    async fn read_bytes(&self, path: &WorkspacePath, limit: u64) -> Result<Vec<u8>, FsError> {
        self.files.read_bytes(&self.follow(path), limit).await
    }

    async fn write(&self, path: &WorkspacePath, contents: &str) -> Result<(), FsError> {
        self.files.write(&self.follow(path), contents).await
    }

    async fn list_dir(&self, path: &WorkspacePath) -> Result<Vec<DirEntry>, FsError> {
        self.files.list_dir(path).await
    }

    async fn exists(&self, path: &WorkspacePath) -> Result<bool, FsError> {
        self.files.exists(&self.follow(path)).await
    }

    async fn remove_file(&self, path: &WorkspacePath) -> Result<(), FsError> {
        self.files.remove_file(path).await
    }

    async fn resolve_real(&self, path: &WorkspacePath) -> Result<WorkspacePath, FsError> {
        Ok(self.follow(path))
    }
}

/// Stands in for `run_command ln -s <to> <from>`: changes the filesystem
/// without planning a write.
struct MakeLink(Arc<LinkingFileSystem>);

#[async_trait]
impl Tool for MakeLink {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: ToolName::new("make_link").unwrap(),
            description: String::new(),
            input_schema: json!({}),
            safety: ToolSafety::Destructive,
        }
    }

    async fn preview(&self, _: &Value) -> Result<Vec<PlannedWrite>, ToolError> {
        Ok(Vec::new())
    }

    async fn execute(&self, arguments: Value) -> Result<ToolOutcome, ToolError> {
        let from = arguments["from"].as_str().unwrap().to_string();
        let to = arguments["to"].as_str().unwrap().to_string();
        self.0.links.lock().unwrap().insert(from, to);
        Ok(ToolOutcome::new("linked"))
    }
}

/// Counts how often the tool it wraps is previewed.
struct CountingPreviews {
    inner: Arc<dyn Tool>,
    previews: Arc<AtomicUsize>,
}

#[async_trait]
impl Tool for CountingPreviews {
    fn definition(&self) -> ToolDefinition {
        self.inner.definition()
    }

    async fn preview(&self, arguments: &Value) -> Result<Vec<PlannedWrite>, ToolError> {
        self.previews.fetch_add(1, Ordering::SeqCst);
        self.inner.preview(arguments).await
    }

    async fn execute(&self, arguments: Value) -> Result<ToolOutcome, ToolError> {
        self.inner.execute(arguments).await
    }
}

/// The gate that would ask a human: answers every question about a write
/// with `decision` and records what it was asked.
struct Human {
    decision: ApprovalDecision,
    asked: Mutex<Vec<ApprovalRequest>>,
    dependency_questions: Mutex<Vec<DependencyApprovalRequest>>,
}

impl Human {
    fn answering(decision: ApprovalDecision) -> Arc<Self> {
        Arc::new(Self {
            decision,
            asked: Mutex::new(Vec::new()),
            dependency_questions: Mutex::new(Vec::new()),
        })
    }

    /// The questions asked about `tool`.
    fn asked_about(&self, tool: &str) -> Vec<ApprovalRequest> {
        self.asked
            .lock()
            .unwrap()
            .iter()
            .filter(|request| request.call.name.as_str() == tool)
            .cloned()
            .collect()
    }
}

#[async_trait]
impl ApprovalGate for Human {
    async fn reauthorize_write(
        &self,
        _request: &ApprovalRequest,
        _writes: &[agent_domain::model::tool::PlannedWrite],
    ) -> Result<ApprovalDecision, ApprovalError> {
        Ok(ApprovalDecision::Approve)
    }

    async fn authorize(
        &self,
        request: &ApprovalRequest,
    ) -> Result<ApprovalDecision, ApprovalError> {
        self.asked.lock().unwrap().push(request.clone());
        // The command that makes the link is always let through: these
        // tests are about the write after it.
        if request.call.name.as_str() == "make_link" {
            return Ok(ApprovalDecision::Approve);
        }
        Ok(self.decision.clone())
    }

    async fn authorize_dependency_change(
        &self,
        request: &DependencyApprovalRequest,
    ) -> Result<ApprovalDecision, ApprovalError> {
        self.dependency_questions
            .lock()
            .unwrap()
            .push(request.clone());
        Ok(self.decision.clone())
    }
}

fn write_rule(path: &str, effect: Effect) -> PermissionRule {
    PermissionRule {
        tool: ToolName::new("write_file").unwrap(),
        subject: Some(SubjectPattern::Path(PathPattern::parse(path).unwrap())),
        effect,
    }
}

struct Turn {
    results: Vec<ToolResult>,
    events: Vec<AgentEvent>,
    previews: usize,
}

/// Runs `make_link` from `link.0` to `link.1`, then `write_file` with
/// `write`, as one batch, under `rules` in front of `human`.
async fn link_then_write(
    file_system: &Arc<LinkingFileSystem>,
    rules: Vec<PermissionRule>,
    human: Arc<Human>,
    link: (&str, &str),
    write: Value,
) -> Turn {
    let previews = Arc::new(AtomicUsize::new(0));
    let tools = Arc::new(
        ToolRegistry::new()
            .with(Arc::new(MakeLink(file_system.clone())))
            .with(Arc::new(CountingPreviews {
                inner: Arc::new(WriteFileTool::new(file_system.clone(), root())),
                previews: previews.clone(),
            })),
    );
    let gate =
        RuleGate::new(human, PermissionRules::new(rules), tools.clone(), root()).expect("rules");
    let events = Arc::new(RecordingSink::default());
    let batch = calls(&[
        ("c1", "make_link", json!({"from": link.0, "to": link.1})),
        ("c2", "write_file", write),
    ]);

    let results = dispatcher(tools, Arc::new(gate), events.clone())
        .dispatch(&batch, &Interrupt::new())
        .await;

    let events = events.events.lock().unwrap().clone();
    Turn {
        results,
        events,
        previews: previews.load(Ordering::SeqCst),
    }
}

#[tokio::test]
async fn a_link_made_earlier_in_the_turn_cannot_carry_a_write_into_a_denied_file() {
    let file_system = LinkingFileSystem::with(&[("secrets/token", "keep")]);
    let human = Human::answering(ApprovalDecision::Approve);

    let turn = link_then_write(
        &file_system,
        vec![write_rule("secrets/", Effect::Deny)],
        human.clone(),
        ("notes/x", "secrets/token"),
        json!({"path": "notes/x", "content": "overwritten"}),
    )
    .await;

    assert_eq!(turn.results.len(), 2, "every call is answered");
    assert!(!turn.results[0].is_error, "{:?}", turn.results[0]);
    let refused = &turn.results[1];
    assert!(refused.is_error, "{refused:?}");
    assert!(
        refused
            .content
            .contains("checked again right before it ran")
            && refused.content.contains("deny write_file path `secrets/`")
            && refused.content.contains("Nothing was written"),
        "the model is told what refused it and when: {}",
        refused.content
    );
    assert_eq!(file_system.get("secrets/token").as_deref(), Some("keep"));
    assert!(
        turn.events.iter().any(|event| matches!(
            event,
            AgentEvent::ToolCallDenied { name, .. } if name.as_str() == "write_file"
        )),
        "shown as a denial: {:?}",
        turn.events
    );
}

#[tokio::test]
async fn a_write_allowed_at_first_is_asked_about_when_it_now_lands_on_an_ask_rule() {
    for (decision, written) in [
        (ApprovalDecision::Approve, true),
        (
            ApprovalDecision::Deny {
                reason: "the user answered no".into(),
            },
            false,
        ),
    ] {
        let file_system = LinkingFileSystem::with(&[("secrets/token", "keep")]);
        let human = Human::answering(decision);

        let turn = link_then_write(
            &file_system,
            vec![
                write_rule("notes/", Effect::Allow),
                write_rule("secrets/", Effect::Ask),
            ],
            human.clone(),
            ("notes/x", "secrets/token"),
            json!({"path": "notes/x", "content": "overwritten"}),
        )
        .await;

        let asked = human.asked_about("write_file");
        assert_eq!(
            asked.len(),
            1,
            "allowed at first without asking, then asked once: {asked:?}"
        );
        assert!(asked[0].must_ask, "an ask rule is never answered by policy");
        assert!(
            asked[0].summary.contains("secrets/token"),
            "the human is told where the write lands: {}",
            asked[0].summary
        );
        let content = file_system.get("secrets/token");
        if written {
            assert!(!turn.results[1].is_error, "{:?}", turn.results[1]);
            assert_eq!(content.as_deref(), Some("overwritten"));
        } else {
            assert!(turn.results[1].is_error, "{:?}", turn.results[1]);
            assert!(
                turn.results[1].content.contains("the user answered no"),
                "{}",
                turn.results[1].content
            );
            assert_eq!(content.as_deref(), Some("keep"));
        }
    }
}

#[tokio::test]
async fn an_ask_answered_at_first_is_not_asked_again() {
    let file_system = LinkingFileSystem::with(&[]);
    let human = Human::answering(ApprovalDecision::Approve);

    let turn = link_then_write(
        &file_system,
        vec![write_rule("notes/", Effect::Ask)],
        human.clone(),
        ("elsewhere", "unrelated"),
        json!({"path": "notes/x", "content": "hello"}),
    )
    .await;

    assert!(!turn.results[1].is_error, "{:?}", turn.results[1]);
    assert_eq!(human.asked_about("write_file").len(), 1);
    assert_eq!(file_system.get("notes/x").as_deref(), Some("hello"));
}

#[tokio::test]
async fn a_target_an_allow_rule_no_longer_covers_goes_to_the_ordinary_approval() {
    let file_system = LinkingFileSystem::with(&[("README.md", "# demo")]);
    let human = Human::answering(ApprovalDecision::Approve);

    link_then_write(
        &file_system,
        vec![write_rule("notes/", Effect::Allow)],
        human.clone(),
        ("notes/x", "README.md"),
        json!({"path": "notes/x", "content": "overwritten"}),
    )
    .await;

    let asked = human.asked_about("write_file");
    assert_eq!(asked.len(), 1, "{asked:?}");
    assert!(
        !asked[0].must_ask,
        "no rule demands a human; the ordinary policy decides"
    );
    assert!(
        asked[0].summary.contains("README.md"),
        "{}",
        asked[0].summary
    );
}

#[tokio::test]
async fn one_preview_serves_the_rules_and_the_dependency_question() {
    let manifest = "[package]\nname = \"demo\"\n\n[dependencies]\n";
    let file_system = LinkingFileSystem::with(&[("Cargo.toml", manifest)]);
    let human = Human::answering(ApprovalDecision::Approve);

    let turn = link_then_write(
        &file_system,
        vec![
            write_rule("notes/", Effect::Allow),
            write_rule("Cargo.toml", Effect::Ask),
        ],
        human.clone(),
        ("notes/deps", "Cargo.toml"),
        json!({
            "path": "notes/deps",
            "content": format!("{manifest}tokio = \"1\"\n"),
        }),
    )
    .await;

    assert!(!turn.results[1].is_error, "{:?}", turn.results[1]);
    assert_eq!(
        turn.previews, 2,
        "one preview for the first approval, one shared right before the call"
    );
    let asked = human.asked_about("write_file");
    assert_eq!(asked.len(), 1, "{asked:?}");
    assert!(asked[0].must_ask);
    let dependency_questions = human.dependency_questions.lock().unwrap();
    assert_eq!(dependency_questions.len(), 1, "both questions were asked");
    assert_eq!(
        dependency_questions[0].changes[0].target,
        Some(WorkspacePath::parse("Cargo.toml").unwrap())
    );
}
