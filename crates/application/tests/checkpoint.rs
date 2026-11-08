//! `/undo` and `/rewind` through the real loop: which writes belong to which
//! turn, what is put back, and what is left alone.

mod common;

use std::sync::{Arc, Mutex};

use agent_application::agent::{AgentLoop, Session};
use agent_application::checkpoint::{CheckpointError, CheckpointJournal, CheckpointingTool};
use agent_application::tools::ToolRegistry;
use agent_application::tools::exec::StartCommandTool;
use agent_application::tools::file::{EditFileTool, WriteFileTool};
use agent_domain::error::{JobError, ToolError};
use agent_domain::model::message::Role;
use agent_domain::model::tool::{
    Before, PlannedWrite, ToolDefinition, ToolName, ToolOutcome, ToolSafety,
};
use agent_domain::model::workspace::WorkspacePath;
use agent_domain::ports::background::{BackgroundCommands, JobId, JobLimits, JobOutput};
use agent_domain::ports::command::{CommandRequest, SandboxKind};
use agent_domain::ports::conversation_log::ConversationLog;
use agent_domain::ports::file_system::FileSystem;
use agent_domain::ports::tool::Tool;
use async_trait::async_trait;
use common::*;
use serde_json::{Value, json};

/// Writes and edits wrapped as the composition root wraps them, plus any
/// `extra` tools, wrapped the same way.
fn checkpointed(
    file_system: &Arc<MemoryFileSystem>,
    journal: &Arc<CheckpointJournal>,
    extra: Vec<Arc<dyn Tool>>,
) -> Arc<ToolRegistry> {
    let mut tools = ToolRegistry::new()
        .with(Arc::new(WriteFileTool::new(file_system.clone(), root())))
        .with(Arc::new(EditFileTool::new(file_system.clone(), root())));
    for tool in extra {
        tools.register(tool);
    }
    Arc::new(tools.map(|_, tool| CheckpointingTool::wrap(tool, journal.clone())))
}

struct Harness {
    files: Arc<MemoryFileSystem>,
    journal: Arc<CheckpointJournal>,
    log: Arc<RecordingLog>,
    agent: AgentLoop,
    provider: Arc<ScriptedProvider>,
}

fn harness(
    files: &[(&str, &str)],
    script: Vec<agent_domain::model::llm::ChatResponse>,
    extra: Vec<Arc<dyn Tool>>,
) -> Harness {
    let files = MemoryFileSystem::with(files);
    let journal = Arc::new(CheckpointJournal::new(files.clone()));
    let log = Arc::new(RecordingLog::default());
    let provider = ScriptedProvider::new(script);
    let agent = loop_with_log(
        provider.clone(),
        checkpointed(&files, &journal, extra),
        log.clone(),
    )
    .with_checkpoints(journal.clone());
    Harness {
        files,
        journal,
        log,
        agent,
        provider,
    }
}

fn write(id: &str, path: &str, content: &str) -> agent_domain::model::llm::ChatResponse {
    tool_use(id, "write_file", json!({"path": path, "content": content}))
}

fn path(raw: &str) -> WorkspacePath {
    WorkspacePath::parse(raw).unwrap()
}

#[tokio::test]
async fn undo_takes_back_only_the_newest_turn_and_keeps_the_conversation() {
    let h = harness(
        &[("a.txt", "v0")],
        vec![
            write("c1", "a.txt", "v1"),
            assistant("first done"),
            write("c2", "a.txt", "v2"),
            assistant("second done"),
        ],
        Vec::new(),
    );
    let mut session = Session::new("s");
    h.agent.run(&mut session, "first").await.unwrap();
    h.agent.run(&mut session, "second").await.unwrap();
    let messages = session.conversation.len();

    let report = h.agent.undo(&mut session).await.unwrap();

    assert_eq!(h.files.get("a.txt").as_deref(), Some("v1"));
    assert_eq!(report.restored, [path("a.txt")]);
    assert_eq!(
        session.conversation.len(),
        messages,
        "/undo leaves the conversation alone"
    );

    h.agent.undo(&mut session).await.unwrap();
    assert_eq!(
        h.files.get("a.txt").as_deref(),
        Some("v0"),
        "a second /undo reaches the turn before"
    );
    assert_eq!(
        h.agent.undo(&mut session).await.unwrap_err(),
        CheckpointError::NothingToUndo
    );
}

#[tokio::test]
async fn a_turn_starts_where_the_loop_pushed_the_user_message() {
    let h = harness(
        &[],
        vec![assistant("nothing to do"), write("c1", "new.txt", "x")],
        Vec::new(),
    );
    let mut session = Session::new("s");
    h.agent.run(&mut session, "hello").await.unwrap();
    h.agent.run(&mut session, "write it").await.unwrap();

    let user_seqs: Vec<u64> = session
        .conversation
        .messages()
        .iter()
        .filter(|message| message.role == Role::User)
        .filter_map(|message| message.seq)
        .collect();
    assert_eq!(h.journal.turns(), 2);
    assert_eq!(h.journal.turn_seq(1), Some(user_seqs[1]));
    assert_eq!(h.journal.turn_seq(2), Some(user_seqs[0]));
    assert_eq!(h.journal.turn_seq(3), None);
}

#[tokio::test]
async fn a_file_the_agent_created_is_deleted_again() {
    let h = harness(
        &[],
        vec![write("c1", "new.txt", "hello"), assistant("done")],
        Vec::new(),
    );
    let mut session = Session::new("s");
    h.agent.run(&mut session, "create it").await.unwrap();

    let report = h.agent.undo(&mut session).await.unwrap();

    assert_eq!(h.files.get("new.txt"), None);
    assert_eq!(report.removed, [path("new.txt")]);
}

#[tokio::test]
async fn a_file_changed_since_is_reported_and_never_overwritten() {
    let h = harness(
        &[("a.txt", "v0")],
        vec![write("c1", "a.txt", "v1"), assistant("done")],
        Vec::new(),
    );
    let mut session = Session::new("s");
    h.agent.run(&mut session, "write").await.unwrap();
    h.files
        .write(&path("a.txt"), "edited by hand")
        .await
        .unwrap();

    let report = h.agent.undo(&mut session).await.unwrap();

    assert_eq!(h.files.get("a.txt").as_deref(), Some("edited by hand"));
    assert_eq!(report.conflicts, [path("a.txt")]);
    assert!(report.restored.is_empty(), "{report:?}");
    assert!(
        report.note_for_model().is_none(),
        "nothing changed, so there is nothing to tell the model"
    );
}

#[tokio::test]
async fn the_model_is_told_what_undo_put_back_on_its_next_turn() {
    let h = harness(
        &[("a.txt", "v0")],
        vec![
            write("c1", "a.txt", "v1"),
            assistant("done"),
            assistant("noted"),
        ],
        Vec::new(),
    );
    let mut session = Session::new("s");
    h.agent.run(&mut session, "write").await.unwrap();
    h.agent.undo(&mut session).await.unwrap();

    h.agent.run(&mut session, "what now?").await.unwrap();

    let sent = h.provider.requests().last().unwrap().messages.clone();
    let user = sent
        .iter()
        .rev()
        .find(|message| message.role == Role::User)
        .unwrap()
        .text();
    assert!(user.contains("/undo"), "{user}");
    assert!(user.contains("a.txt"), "{user}");
    assert!(
        user.ends_with("what now?"),
        "the note goes in front of what the user said: {user}"
    );

    h.agent.run(&mut session, "and then?").await.unwrap();
    let sent = h.provider.requests().last().unwrap().messages.clone();
    assert_eq!(
        sent.last().unwrap().text(),
        "and then?",
        "the note is given once"
    );
}

/// Plans a write and fails without carrying it out, or after carrying out
/// only part of it - what a full disk does to a write.
struct FailingWriter {
    files: Arc<MemoryFileSystem>,
    partial: Option<&'static str>,
}

#[async_trait]
impl Tool for FailingWriter {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: ToolName::new("failing_write").unwrap(),
            description: String::new(),
            input_schema: json!({"type": "object"}),
            safety: ToolSafety::Mutating,
        }
    }

    async fn preview(&self, _arguments: &Value) -> Result<Vec<PlannedWrite>, ToolError> {
        let before = self.files.get("a.txt").map_or(Before::Absent, Before::Text);
        Ok(vec![PlannedWrite {
            path: path("a.txt"),
            target: path("a.txt"),
            before,
            after: "complete".into(),
        }])
    }

    async fn execute(&self, _arguments: Value) -> Result<ToolOutcome, ToolError> {
        if let Some(partial) = self.partial {
            self.files.write(&path("a.txt"), partial).await.unwrap();
        }
        Err(ToolError::execution(
            &self.definition().name,
            "no space left on device",
        ))
    }
}

#[tokio::test]
async fn a_failed_write_that_changed_nothing_is_left_out_of_the_report() {
    let h = harness(
        &[("a.txt", "v0")],
        vec![
            write("c1", "a.txt", "v1"),
            tool_use("c2", "failing_write", json!({})),
            assistant("done"),
        ],
        Vec::new(),
    );
    let failing: Arc<dyn Tool> = Arc::new(FailingWriter {
        files: h.files.clone(),
        partial: None,
    });
    let agent = loop_with_log(
        h.provider.clone(),
        checkpointed(&h.files, &h.journal, vec![failing]),
        h.log.clone(),
    )
    .with_checkpoints(h.journal.clone());
    let mut session = Session::new("s");
    agent.run(&mut session, "write").await.unwrap();

    let report = agent.undo(&mut session).await.unwrap();

    assert_eq!(h.files.get("a.txt").as_deref(), Some("v0"));
    assert_eq!(report.restored, [path("a.txt")]);
    assert!(report.conflicts.is_empty(), "{report:?}");
}

#[tokio::test]
async fn a_failed_write_that_left_half_a_file_is_reported_not_overwritten() {
    let h = harness(
        &[("a.txt", "v0")],
        vec![
            tool_use("c1", "failing_write", json!({})),
            assistant("done"),
        ],
        Vec::new(),
    );
    let failing: Arc<dyn Tool> = Arc::new(FailingWriter {
        files: h.files.clone(),
        partial: Some("compl"),
    });
    let agent = loop_with_log(
        h.provider.clone(),
        checkpointed(&h.files, &h.journal, vec![failing]),
        h.log.clone(),
    )
    .with_checkpoints(h.journal.clone());
    let mut session = Session::new("s");
    agent.run(&mut session, "write").await.unwrap();

    let report = agent.undo(&mut session).await.unwrap();

    assert_eq!(h.files.get("a.txt").as_deref(), Some("compl"));
    assert_eq!(report.conflicts, [path("a.txt")]);
}

/// Stands in for `run_command`: writes nothing it can name in advance.
struct Command(Arc<Mutex<u32>>);

#[async_trait]
impl Tool for Command {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: ToolName::new("run_command").unwrap(),
            description: String::new(),
            input_schema: json!({"type": "object"}),
            safety: ToolSafety::Destructive,
        }
    }

    async fn preview(&self, _arguments: &Value) -> Result<Vec<PlannedWrite>, ToolError> {
        Ok(Vec::new())
    }

    async fn execute(&self, _arguments: Value) -> Result<ToolOutcome, ToolError> {
        *self.0.lock().unwrap() += 1;
        Ok(ToolOutcome::new("ran"))
    }
}

#[tokio::test]
async fn calls_that_cannot_be_undone_are_listed_as_not_undone() {
    let ran = Arc::new(Mutex::new(0));
    let h = harness(
        &[],
        vec![
            tool_use("c1", "run_command", json!({})),
            write("c2", "new.txt", "x"),
            assistant("done"),
        ],
        vec![Arc::new(Command(ran.clone()))],
    );
    let mut session = Session::new("s");
    h.agent.run(&mut session, "go").await.unwrap();

    let report = h.agent.undo(&mut session).await.unwrap();

    assert_eq!(*ran.lock().unwrap(), 1);
    assert_eq!(report.not_undone, [ToolName::new("run_command").unwrap()]);
    assert_eq!(report.removed, [path("new.txt")]);
}

#[tokio::test]
async fn rewind_takes_back_files_and_conversation_and_marks_the_log() {
    let h = harness(
        &[("a.txt", "v0")],
        vec![
            write("c1", "a.txt", "v1"),
            assistant("first done"),
            write("c2", "a.txt", "v2"),
            assistant("second done"),
        ],
        Vec::new(),
    );
    let mut session = Session::new("s");
    h.agent.run(&mut session, "first").await.unwrap();
    let after_first = session.conversation.len();
    h.agent.run(&mut session, "second").await.unwrap();
    let second_seq = h.journal.turn_seq(1).unwrap();

    let report = h.agent.rewind(&mut session, 1).await.unwrap();

    assert_eq!(h.files.get("a.txt").as_deref(), Some("v1"));
    assert_eq!(session.conversation.len(), after_first);
    assert_eq!(
        session.conversation.last_assistant_text().as_deref(),
        Some("first done")
    );
    assert_eq!(report.dropped_messages, 4);
    assert_eq!(
        h.log.rewinds.lock().unwrap().as_slice(),
        [("s".to_string(), second_seq)],
        "a resume must take the same messages back"
    );
    assert_eq!(h.journal.turns(), 1, "the rewound turn is forgotten");
}

#[tokio::test]
async fn rewinding_further_than_was_recorded_changes_nothing() {
    let h = harness(
        &[("a.txt", "v0")],
        vec![write("c1", "a.txt", "v1"), assistant("done")],
        Vec::new(),
    );
    let mut session = Session::new("s");
    h.agent.run(&mut session, "write").await.unwrap();
    let before = session.conversation.clone();

    assert_eq!(
        h.agent.rewind(&mut session, 2).await.unwrap_err(),
        CheckpointError::NotThatManyTurns { recorded: 1 }
    );
    assert_eq!(h.files.get("a.txt").as_deref(), Some("v1"));
    assert_eq!(session.conversation, before);
    assert!(h.log.rewinds.lock().unwrap().is_empty());
}

#[tokio::test]
async fn rewinding_past_an_undo_drops_the_note_about_it() {
    let h = harness(
        &[("a.txt", "v0")],
        vec![
            write("c1", "a.txt", "v1"),
            assistant("done"),
            assistant("fresh start"),
        ],
        Vec::new(),
    );
    let mut session = Session::new("s");
    h.agent.run(&mut session, "write").await.unwrap();
    h.agent.undo(&mut session).await.unwrap();
    h.agent.rewind(&mut session, 1).await.unwrap();

    h.agent.run(&mut session, "again").await.unwrap();

    let sent = h.provider.requests().last().unwrap().messages.clone();
    assert_eq!(sent.len(), 1, "{sent:?}");
    assert_eq!(
        sent[0].text(),
        "again",
        "the turn the note was about is gone"
    );
}

#[tokio::test]
async fn a_loop_without_a_journal_says_so() {
    let files = MemoryFileSystem::with(&[]);
    let agent = loop_with_log(
        ScriptedProvider::new(Vec::new()),
        registry(files),
        Arc::new(RecordingLog::default()) as Arc<dyn ConversationLog>,
    );
    let mut session = Session::new("s");
    assert_eq!(
        agent.undo(&mut session).await.unwrap_err(),
        CheckpointError::Disabled
    );
}

/// A job supervisor that starts nothing - the call is all that matters here.
struct NoJobs;

#[async_trait]
impl BackgroundCommands for NoJobs {
    fn sandbox(&self) -> SandboxKind {
        SandboxKind::LandlockConfined
    }

    fn limits(&self) -> JobLimits {
        JobLimits {
            max_jobs: 1,
            max_lifetime: std::time::Duration::from_secs(60),
            buffer_bytes: 1024,
        }
    }

    async fn start(&self, _request: CommandRequest) -> Result<JobId, JobError> {
        Ok(JobId::new(1))
    }

    async fn read(&self, id: JobId, _since: u64, _max: usize) -> Result<JobOutput, JobError> {
        Err(JobError::Unknown { id: id.get() })
    }

    async fn kill(&self, id: JobId, _max: usize) -> Result<JobOutput, JobError> {
        Err(JobError::Unknown { id: id.get() })
    }
}

/// A background job keeps changing things after its call returns, so nothing
/// it does can be claimed as checkpointed. `start_command` is destructive -
/// the class the composition root wraps for `/undo` - and plans no writes,
/// which is what puts it on the "not undone" list.
#[tokio::test]
async fn a_started_background_job_is_listed_as_not_undone() {
    let tool = StartCommandTool::new(Arc::new(NoJobs), &[]);
    assert_eq!(tool.definition().safety, ToolSafety::Destructive);
    let h = harness(
        &[],
        vec![
            tool_use("c1", "start_command", json!({"command": "npm run dev"})),
            assistant("started"),
        ],
        vec![Arc::new(tool)],
    );
    let mut session = Session::new("s");
    h.agent.run(&mut session, "go").await.unwrap();

    let report = h.agent.undo(&mut session).await.unwrap();

    assert_eq!(report.not_undone, [ToolName::new("start_command").unwrap()]);
    assert!(!report.changed_files());
}
