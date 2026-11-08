//! Plan mode: the model may look but not touch until the user says so.
//!
//! Enforced at the approval gate, by safety class: only
//! [`ToolSafety::ReadOnly`] calls and [`ToolSafety::Network`] lookups
//! (`web_fetch`, `web_search`, which go on to the ordinary approval) are let
//! through. Never by tool name - a tool added later, or one an MCP server
//! offers, is held back by what it can do rather than by whether someone
//! remembered to list it.
//!
//! What the model sees does not change: the tool list and the system prompt
//! stay byte for byte the same, so the provider's prompt cache keeps
//! matching across a switch. The model is told about a switch by a note in
//! front of the user's next message, like the note after an `/undo`, and is
//! told again by the refusal itself if it reaches for a write anyway.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use agent_domain::error::ApprovalError;
use agent_domain::model::tool::ToolSafety;
use agent_domain::ports::approval::{
    ApprovalDecision, ApprovalGate, ApprovalRequest, DependencyApprovalRequest,
};
use async_trait::async_trait;

use crate::agent::Session;

const ENTERED: &str = "[Agent note - this is not a message from the user]\n\
     The user turned plan mode on. Until they turn it off, only tools that do not change \
     anything will run (reading and searching files, web lookups, todo_write); a call that \
     writes files or runs a command is refused. Investigate as much as you need, then present \
     your plan and wait for the user to turn plan mode off before making any change.";

const LEFT: &str = "[Agent note - this is not a message from the user]\n\
     The user turned plan mode off. Tools that write files and run commands work again, with \
     the usual approvals. Go ahead with the changes if the message below asks you to.";

/// Whether plan mode is on. One switch shared by the gate that enforces it
/// and the command that flips it; cloning shares it.
#[derive(Debug, Clone, Default)]
pub struct PlanMode(Arc<AtomicBool>);

impl PlanMode {
    /// Off.
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_on(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }

    /// Turns plan mode on or off and tells the model so, in front of the
    /// user's next message in `session`. Returns whether anything changed.
    ///
    /// The model hears about each switch once. Switching back before it has
    /// heard about the first one takes the first note away instead of adding
    /// a second: as far as the model knows nothing changed, and two notes
    /// that cancel out are noise.
    pub fn set(&self, session: &mut Session, on: bool) -> bool {
        if self.0.swap(on, Ordering::SeqCst) == on {
            return false;
        }
        let (now, before) = if on { (ENTERED, LEFT) } else { (LEFT, ENTERED) };
        let pending = session.notes.len();
        session.notes.retain(|(_, note)| note != before);
        if session.notes.len() == pending {
            // Not about any turn: a rewind takes conversation back, not the
            // mode, so the note has to survive one. Only a rewind to the very
            // first message (sequence 0) drops it; the refusal still explains.
            session.notes.push((0, now.to_string()));
        }
        true
    }
}

/// Refuses every call that is not read-only while plan mode is on; asks
/// `inner` about everything else.
pub struct PlanModeGate {
    inner: Arc<dyn ApprovalGate>,
    mode: PlanMode,
}

impl PlanModeGate {
    pub fn new(inner: Arc<dyn ApprovalGate>, mode: PlanMode) -> Self {
        Self { inner, mode }
    }
}

/// Network calls observe; they do not change the workspace. root decision:
/// they stay usable while planning, behind the ordinary approval.
fn allowed_while_planning(safety: ToolSafety) -> bool {
    safety.is_read_only() || safety == ToolSafety::Network
}

#[async_trait]
impl ApprovalGate for PlanModeGate {
    async fn authorize(
        &self,
        request: &ApprovalRequest,
    ) -> Result<ApprovalDecision, ApprovalError> {
        if self.mode.is_on() && !allowed_while_planning(request.safety) {
            return Ok(ApprovalDecision::Deny {
                reason: format!(
                    "plan mode is on, so `{}` ({}) cannot run - only tools that change nothing \
                     do. Present your plan to the user instead, and wait for them to turn plan \
                     mode off before making changes",
                    request.call.name,
                    request.safety.label()
                ),
            });
        }
        self.inner.authorize(request).await
    }

    async fn authorize_dependency_change(
        &self,
        request: &DependencyApprovalRequest,
    ) -> Result<ApprovalDecision, ApprovalError> {
        self.inner.authorize_dependency_change(request).await
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use agent_domain::model::dependency::{
        DependencyChangeDescription, ManifestChange, ManifestKind,
    };
    use agent_domain::model::tool::{Before, ToolCall, ToolCallId, ToolName};
    use serde_json::json;

    use super::*;

    /// Approves everything and remembers what it was asked.
    #[derive(Default)]
    struct Recording {
        calls: Mutex<Vec<String>>,
        dependency_questions: Mutex<usize>,
    }

    #[async_trait]
    impl ApprovalGate for Recording {
        async fn authorize(
            &self,
            request: &ApprovalRequest,
        ) -> Result<ApprovalDecision, ApprovalError> {
            self.calls
                .lock()
                .unwrap()
                .push(request.call.name.to_string());
            Ok(ApprovalDecision::Approve)
        }

        async fn authorize_dependency_change(
            &self,
            _request: &DependencyApprovalRequest,
        ) -> Result<ApprovalDecision, ApprovalError> {
            *self.dependency_questions.lock().unwrap() += 1;
            Ok(ApprovalDecision::Approve)
        }
    }

    fn request(tool: &str, safety: ToolSafety) -> ApprovalRequest {
        ApprovalRequest {
            call: ToolCall::new(
                ToolCallId::new("1"),
                ToolName::new(tool).unwrap(),
                json!({}),
            ),
            safety,
            summary: tool.to_string(),
            must_ask: false,
        }
    }

    fn planning() -> (PlanModeGate, Arc<Recording>, Session) {
        let inner = Arc::new(Recording::default());
        let mode = PlanMode::new();
        let mut session = Session::new("s");
        mode.set(&mut session, true);
        (PlanModeGate::new(inner.clone(), mode), inner, session)
    }

    #[tokio::test]
    async fn writes_and_commands_are_refused_while_planning() {
        let (gate, inner, _) = planning();
        for (tool, safety) in [
            ("write_file", ToolSafety::Mutating),
            ("run_command", ToolSafety::Destructive),
            ("mcp__db__drop", ToolSafety::Destructive),
        ] {
            match gate.authorize(&request(tool, safety)).await.unwrap() {
                ApprovalDecision::Deny { reason } => {
                    assert!(reason.contains("plan mode is on"), "{reason}");
                    assert!(reason.contains("Present your plan"), "{reason}");
                }
                other => panic!("`{tool}` must be refused, got {other:?}"),
            }
        }
        assert!(
            inner.calls.lock().unwrap().is_empty(),
            "a refused call never reaches the human"
        );
    }

    #[tokio::test]
    async fn reads_lookups_and_the_plan_go_on_to_the_ordinary_approval() {
        let (gate, inner, _) = planning();
        for (tool, safety) in [
            ("read_file", ToolSafety::ReadOnly),
            ("todo_write", ToolSafety::ReadOnly),
            ("web_fetch", ToolSafety::Network),
        ] {
            let decision = gate.authorize(&request(tool, safety)).await.unwrap();
            assert!(decision.is_approved(), "`{tool}`: {decision:?}");
        }
        assert_eq!(
            *inner.calls.lock().unwrap(),
            ["read_file", "todo_write", "web_fetch"]
        );
    }

    #[tokio::test]
    async fn off_means_every_call_goes_to_the_inner_gate() {
        let (gate, inner, mut session) = planning();
        gate.mode.set(&mut session, false);
        let decision = gate
            .authorize(&request("write_file", ToolSafety::Mutating))
            .await
            .unwrap();
        assert!(decision.is_approved());
        assert_eq!(inner.calls.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn the_dependency_question_is_forwarded_to_the_inner_gate() {
        let (gate, inner, _) = planning();
        let request = DependencyApprovalRequest {
            call: request("edit_file", ToolSafety::Mutating).call,
            safety: ToolSafety::Mutating,
            changes: vec![ManifestChange {
                path: agent_domain::model::workspace::WorkspacePath::parse("Cargo.toml").unwrap(),
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
        };
        let decision = gate.authorize_dependency_change(&request).await.unwrap();
        assert!(decision.is_approved(), "{decision:?}");
        assert_eq!(*inner.dependency_questions.lock().unwrap(), 1);
    }

    #[test]
    fn each_switch_leaves_one_note_and_a_switch_back_unheard_leaves_none() {
        let mode = PlanMode::new();
        let mut session = Session::new("s");

        assert!(!mode.set(&mut session, false), "already off");
        assert!(session.notes.is_empty());

        assert!(mode.set(&mut session, true));
        assert!(!mode.set(&mut session, true), "already on");
        assert_eq!(session.notes.len(), 1);
        assert!(session.notes[0].1.contains("plan mode on"));

        assert!(mode.set(&mut session, false));
        assert!(
            session.notes.is_empty(),
            "the model never heard it was on: {:?}",
            session.notes
        );

        mode.set(&mut session, true);
        let first = session.with_notes("go".into());
        assert!(first.starts_with(ENTERED), "{first}");
        mode.set(&mut session, false);
        assert_eq!(session.notes.len(), 1, "heard about on, so hears about off");
        assert!(session.notes[0].1.contains("plan mode off"));
    }
}
