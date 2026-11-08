//! Append-only conversation log, one JSON object per line.
//!
//! JSONL rather than one JSON document per session, for the property that
//! matters in a crash: a file that stops mid-line is still readable up to the
//! last complete line, whereas a truncated JSON array is not readable at all.
//! It is also the shape that lets the file be appended to without reading it
//! back first, so the cost of logging does not grow with the length of the
//! session.
//!
//! Every line carries a schema version. The record will grow, and a reader
//! that meets a version it does not know needs to be able to say so rather
//! than guess.

use std::path::{Path, PathBuf};

use agent_domain::error::FsError;
use agent_domain::model::message::Message;
use agent_domain::ports::conversation_log::ConversationLog;
use async_trait::async_trait;
use serde::Serialize;
use tokio::io::AsyncWriteExt as _;

use super::{SCHEMA_VERSION, io_error, path_for};

pub struct FileConversationLog {
    directory: PathBuf,
}

impl FileConversationLog {
    pub fn new(directory: impl Into<PathBuf>) -> Self {
        Self {
            directory: directory.into(),
        }
    }

    pub fn directory(&self) -> &Path {
        &self.directory
    }
}

/// One line of the log.
///
/// The message is flattened in rather than nested under a key, so a line reads
/// as "a message, plus who and when" instead of as an envelope to be unwrapped.
#[derive(Serialize)]
struct Record<'a> {
    v: u32,
    session: &'a str,
    /// Wall-clock time, for the human reading the file later.
    ///
    /// The only clock in the history path, and deliberately on this side of
    /// it: a record is *about* when things happened, while the retention
    /// policy weighs messages by order and size and must never consult a clock
    /// (see `Conversation`). Nothing reads this field back.
    at: String,
    #[serde(flatten)]
    message: &'a Message,
}

/// The line that takes back every message numbered `rewind_to` or later.
#[derive(Serialize)]
struct RewindRecord<'a> {
    v: u32,
    session: &'a str,
    at: String,
    rewind_to: u64,
}

impl FileConversationLog {
    /// Appends `batch` - whole lines - to the record of `session_id`.
    async fn write_lines(&self, session_id: &str, batch: &str) -> Result<(), FsError> {
        let path = path_for(&self.directory, session_id);
        // The file's own directory: a sub-agent's record is a level or two
        // below the session directory.
        let parent = path.parent().unwrap_or(&self.directory);
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|error| io_error(parent, error))?;

        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .await
            .map_err(|error| io_error(&path, error))?;

        file.write_all(batch.as_bytes())
            .await
            .map_err(|error| io_error(&path, error))?;
        file.flush().await.map_err(|error| io_error(&path, error))?;

        Ok(())
    }
}

fn serialise(session_id: &str, record: &impl Serialize) -> Result<String, FsError> {
    serde_json::to_string(record).map_err(|error| FsError::Io {
        path: session_id.to_string(),
        message: format!("cannot serialise a log record: {error}"),
    })
}

#[async_trait]
impl ConversationLog for FileConversationLog {
    async fn append(&self, session_id: &str, messages: &[Message]) -> Result<(), FsError> {
        if messages.is_empty() {
            return Ok(());
        }

        // Rendered in full before the file is touched. A serialisation failure
        // must not leave half a batch on disk, and the write itself is one
        // syscall rather than one per message.
        let at = chrono::Utc::now().to_rfc3339();
        let mut batch = String::new();
        for message in messages {
            let record = Record {
                v: SCHEMA_VERSION,
                session: session_id,
                at: at.clone(),
                message,
            };
            batch.push_str(&serialise(session_id, &record)?);
            batch.push('\n');
        }

        self.write_lines(session_id, &batch).await
    }

    async fn rewind(&self, session_id: &str, seq: u64) -> Result<(), FsError> {
        let record = RewindRecord {
            v: SCHEMA_VERSION,
            session: session_id,
            at: chrono::Utc::now().to_rfc3339(),
            rewind_to: seq,
        };
        let mut line = serialise(session_id, &record)?;
        line.push('\n');
        self.write_lines(session_id, &line).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_domain::model::tool::{ToolCall, ToolCallId, ToolName, ToolResult};
    use serde_json::Value;

    fn lines(path: &Path) -> Vec<Value> {
        std::fs::read_to_string(path)
            .expect("the log must exist")
            .lines()
            .map(|line| serde_json::from_str(line).expect("every line must be valid JSON"))
            .collect()
    }

    #[tokio::test]
    async fn writes_one_line_per_message_and_keeps_appending() {
        let directory = tempfile::tempdir().unwrap();
        let log = FileConversationLog::new(directory.path());

        log.append("s1", &[Message::user("hello")]).await.unwrap();
        log.append(
            "s1",
            &[Message::assistant_text("hi"), Message::user("still here?")],
        )
        .await
        .unwrap();

        let records = lines(&directory.path().join("s1.jsonl"));
        assert_eq!(
            records.len(),
            3,
            "a second call must not overwrite the first"
        );
        assert_eq!(records[0]["role"], "user");
        assert_eq!(records[0]["content"][0]["text"], "hello");
        assert_eq!(records[2]["content"][0]["text"], "still here?");
    }

    #[tokio::test]
    async fn every_line_carries_the_session_and_a_schema_version() {
        let directory = tempfile::tempdir().unwrap();
        let log = FileConversationLog::new(directory.path());

        log.append("s1", &[Message::user("hello")]).await.unwrap();

        let records = lines(&directory.path().join("s1.jsonl"));
        assert_eq!(records[0]["v"], SCHEMA_VERSION);
        assert_eq!(records[0]["session"], "s1");
        assert!(
            records[0]["at"].as_str().is_some_and(|at| at.contains('T')),
            "a record needs a timestamp to be worth reading: {:?}",
            records[0]["at"]
        );
    }

    #[tokio::test]
    async fn tool_activity_is_recorded_whole() {
        // The reason this port exists rather than a logging EventSink: the
        // event for a tool result carries a one-line summary, the record has
        // to carry the result.
        let directory = tempfile::tempdir().unwrap();
        let log = FileConversationLog::new(directory.path());
        let call = ToolCall::new(
            ToolCallId::new("c1"),
            ToolName::new("read_file").unwrap(),
            serde_json::json!({"path": "a.rs"}),
        );

        log.append(
            "s1",
            &[Message::tool_results(vec![ToolResult::ok(
                &call,
                "fn main() {}",
            )])],
        )
        .await
        .unwrap();

        let records = lines(&directory.path().join("s1.jsonl"));
        assert_eq!(records[0]["content"][0]["content"], "fn main() {}");
    }

    #[tokio::test]
    async fn a_session_id_cannot_escape_the_directory() {
        let parent = tempfile::tempdir().unwrap();
        let directory = parent.path().join("sessions");
        let log = FileConversationLog::new(&directory);

        log.append("../../escaped", &[Message::user("hello")])
            .await
            .unwrap();

        assert!(
            !parent.path().join("escaped.jsonl").exists(),
            "a traversing id must not write outside the log directory"
        );
        let written: Vec<_> = std::fs::read_dir(&directory)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(written.len(), 1, "{written:?}");
        assert_eq!(written[0], "______escaped.jsonl");
    }

    #[tokio::test]
    async fn a_rewind_is_appended_as_a_mark_and_leaves_earlier_lines_alone() {
        let directory = tempfile::tempdir().unwrap();
        let log = FileConversationLog::new(directory.path());
        log.append("s1", &[Message::user("hello").with_seq(0)])
            .await
            .unwrap();
        let path = directory.path().join("s1.jsonl");
        let before = std::fs::read_to_string(&path).unwrap();

        log.rewind("s1", 0).await.unwrap();

        let after = std::fs::read_to_string(&path).unwrap();
        assert!(
            after.starts_with(&before),
            "what was written stays as it was: {after}"
        );
        let records = lines(&path);
        assert_eq!(records.len(), 2);
        assert_eq!(records[1]["rewind_to"], 0);
        assert_eq!(records[1]["v"], SCHEMA_VERSION);
        assert!(records[1].get("role").is_none(), "{:?}", records[1]);
    }

    #[tokio::test]
    async fn an_empty_batch_does_not_create_a_file() {
        let directory = tempfile::tempdir().unwrap();
        let log = FileConversationLog::new(directory.path().join("sessions"));

        log.append("s1", &[]).await.unwrap();

        assert!(!directory.path().join("sessions").exists());
    }
}
