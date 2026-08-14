//! Reading sessions back out of the log directory.
//!
//! Resuming is a replay: the lines are pushed into a fresh [`Conversation`] in
//! the order they were written, and the ordinary history policy takes it from
//! there. That is why the log holds *everything* rather than only what fitted
//! in the last request - a resumed session gets the whole record and is
//! compacted on its first turn, exactly as it would have been had it never
//! stopped.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use agent_domain::error::FsError;
use agent_domain::model::conversation::Conversation;
use agent_domain::model::message::{Message, Role};
use agent_domain::ports::session_store::{SessionStore, SessionSummary};
use agent_domain::text;
use async_trait::async_trait;
use serde::Deserialize;
use tokio::io::AsyncReadExt as _;
use tracing::warn;

use super::{SCHEMA_VERSION, io_error, path_for};

/// How much of the opening request a listing shows.
const PREVIEW_BYTES: usize = 72;

/// How far into a record the opening request is looked for.
///
/// A listing must not cost what the sessions weigh, so it reads the front of
/// each file and stops. An opening request longer than this - a pasted stack
/// trace, a whole file quoted into the prompt - simply gets no preview, which
/// is a better trade than reading half a gigabyte to render a table.
const PREVIEW_SCAN_BYTES: usize = 8 * 1024;

/// When a session stops being worth keeping.
///
/// Wall-clock age is the right measure *here*, unlike in the history policy
/// next door, which weighs messages by order and never consults a clock. The
/// difference is what the two are for: one decides how much of a conversation
/// the model still sees, the other decides how long a transcript stays on
/// somebody's disk. The second is a retention question, and retention is
/// stated in days.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RetentionPolicy {
    /// Most sessions to keep, newest first. `None` keeps any number.
    pub keep: Option<usize>,
    /// Oldest a session may be. `None` keeps them forever.
    pub max_age: Option<Duration>,
}

impl RetentionPolicy {
    /// True when nothing would ever be removed.
    pub fn keeps_everything(&self) -> bool {
        self.keep.is_none() && self.max_age.is_none()
    }
}

/// One session file, as its metadata describes it.
struct Entry {
    id: String,
    path: PathBuf,
    modified: SystemTime,
    bytes: u64,
}

pub struct FileSessionStore {
    directory: PathBuf,
}

impl FileSessionStore {
    pub fn new(directory: impl Into<PathBuf>) -> Self {
        Self {
            directory: directory.into(),
        }
    }

    /// Every session file, newest first, described by its metadata alone.
    ///
    /// Nothing here opens a file. That is what lets listing and pruning stay
    /// proportional to the number of sessions rather than to their size.
    ///
    /// A missing directory is an empty list, not an error: it only means
    /// nothing has been written yet, and the first run of a new workspace
    /// should not have to explain itself.
    async fn entries(&self) -> Result<Vec<Entry>, FsError> {
        let mut reader = match tokio::fs::read_dir(&self.directory).await {
            Ok(reader) => reader,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(io_error(&self.directory, error)),
        };

        let mut found: Vec<Entry> = Vec::new();
        while let Some(entry) = reader
            .next_entry()
            .await
            .map_err(|error| io_error(&self.directory, error))?
        {
            let path = entry.path();
            if path.extension().and_then(|extension| extension.to_str()) != Some("jsonl") {
                continue;
            }
            let Some(id) = path.file_stem().and_then(|stem| stem.to_str()) else {
                continue;
            };
            // A file whose metadata cannot be read still belongs in the list.
            // It sorts as the oldest thing there, which also makes it the
            // first thing retention removes - and a session nobody can stat is
            // not one anybody is going to resume.
            let metadata = entry.metadata().await.ok();
            found.push(Entry {
                id: id.to_string(),
                path,
                modified: metadata
                    .as_ref()
                    .and_then(|metadata| metadata.modified().ok())
                    .unwrap_or(SystemTime::UNIX_EPOCH),
                bytes: metadata.map_or(0, |metadata| metadata.len()),
            });
        }

        found.sort_by(|left, right| {
            right
                .modified
                .cmp(&left.modified)
                .then_with(|| left.id.cmp(&right.id))
        });
        Ok(found)
    }

    /// Deletes the sessions `policy` no longer covers, returning their ids.
    ///
    /// An inherent method rather than part of [`SessionStore`]. How long a
    /// transcript lives on a disk is a property of the storage, not a rule
    /// about conversations, and putting it on the port would mean giving the
    /// domain a retention policy - and a unit of days - that nothing in it
    /// would otherwise need.
    ///
    /// `protected` names sessions that survive both rules. The caller passes
    /// the one it is about to resume: a record that is old enough to prune is
    /// exactly the kind somebody reaches for by id, and deleting it moments
    /// before opening it would be the worst possible timing.
    ///
    /// One file that will not delete does not stop the others.
    pub async fn prune(
        &self,
        policy: &RetentionPolicy,
        protected: &[&str],
    ) -> Result<Vec<String>, FsError> {
        if policy.keeps_everything() {
            return Ok(Vec::new());
        }

        let now = SystemTime::now();
        let mut removed = Vec::new();

        for (index, entry) in self.entries().await?.into_iter().enumerate() {
            if protected.contains(&entry.id.as_str()) {
                continue;
            }

            let too_many = policy.keep.is_some_and(|keep| index >= keep);
            // A clock that jumped backwards makes `duration_since` fail, and
            // an age of zero keeps the file. Retention errs towards keeping.
            let too_old = policy.max_age.is_some_and(|max_age| {
                now.duration_since(entry.modified).unwrap_or_default() > max_age
            });

            if !(too_many || too_old) {
                continue;
            }

            match tokio::fs::remove_file(&entry.path).await {
                Ok(()) => removed.push(entry.id),
                Err(error) => {
                    warn!(session = entry.id, %error, "cannot delete this session; leaving it")
                }
            }
        }

        Ok(removed)
    }
}

/// One line as it comes back off disk.
///
/// Only the parts a replay needs. Everything else a line carries - the session
/// id it repeats, the wall-clock time it was written - is there for whoever
/// reads the file with their own eyes.
#[derive(Deserialize)]
struct StoredRecord {
    #[serde(default)]
    v: u32,
    #[serde(flatten)]
    message: Message,
}

/// Rebuilds a conversation from the lines of a log.
///
/// Damage ends the replay rather than being skipped over. Skipping looks more
/// forgiving and is worse: a `tool_result` whose `tool_call` was on the
/// unreadable line becomes an orphan, and the provider rejects the whole
/// request on the next turn. Stopping keeps a prefix, and a prefix of a
/// well-formed history is well formed.
fn replay(session_id: &str, text: &str) -> Conversation {
    let mut conversation = Conversation::new();

    for (number, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<StoredRecord>(line) {
            Ok(record) if (1..=SCHEMA_VERSION).contains(&record.v) => {
                conversation.push(record.message);
            }
            Ok(record) => {
                warn!(
                    session = session_id,
                    line = number + 1,
                    version = record.v,
                    known = SCHEMA_VERSION,
                    "the session log was written by a newer agent; resuming what came before it"
                );
                break;
            }
            Err(error) => {
                warn!(
                    session = session_id,
                    line = number + 1,
                    %error,
                    "the session log is damaged from here on; resuming what came before it"
                );
                break;
            }
        }
    }

    // A record that stops between a tool call and its result would otherwise
    // be sent onwards with the call unanswered.
    conversation.drop_trailing_unanswered_calls();
    conversation
}

/// Opening request of a record, read from the front of the file.
///
/// Only whole lines within the scan window are considered: a line cut off by
/// the window is not JSON, and half a request is worse than none. Returns an
/// empty string when nothing usable is near the start, which is the honest
/// answer - the alternative is reading the whole file to render one column.
async fn preview(path: &Path) -> String {
    let Ok(mut file) = tokio::fs::File::open(path).await else {
        return String::new();
    };

    let mut head = vec![0u8; PREVIEW_SCAN_BYTES];
    let Ok(read) = file.read(&mut head).await else {
        return String::new();
    };
    head.truncate(read);

    let Ok(head) = String::from_utf8(head) else {
        return String::new();
    };

    // `rfind` rather than `lines()`: the last line of the window is very
    // probably truncated, and parsing it would be parsing a fragment.
    let complete = &head[..head.rfind('\n').map_or(0, |end| end + 1)];

    complete
        .lines()
        .filter_map(|line| serde_json::from_str::<StoredRecord>(line).ok())
        .find(|record| record.message.role == Role::User)
        .map(|record| text::first_line(&record.message.text(), PREVIEW_BYTES))
        .unwrap_or_default()
}

#[async_trait]
impl SessionStore for FileSessionStore {
    async fn list(&self) -> Result<Vec<SessionSummary>, FsError> {
        let mut summaries = Vec::new();

        for entry in self.entries().await? {
            summaries.push(SessionSummary {
                preview: preview(&entry.path).await,
                bytes: entry.bytes,
                id: entry.id,
            });
        }

        Ok(summaries)
    }

    async fn load(&self, session_id: &str) -> Result<Conversation, FsError> {
        let path = path_for(&self.directory, session_id);
        let text = tokio::fs::read_to_string(&path).await.map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                FsError::NotFound {
                    path: path.display().to_string(),
                }
            } else {
                io_error(&path, error)
            }
        })?;

        Ok(replay(session_id, &text))
    }

    async fn latest(&self) -> Result<Option<String>, FsError> {
        Ok(self
            .entries()
            .await?
            .into_iter()
            .next()
            .map(|entry| entry.id))
    }

    async fn delete(&self, session_id: &str) -> Result<(), FsError> {
        let path = path_for(&self.directory, session_id);
        match tokio::fs::remove_file(&path).await {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Err(FsError::NotFound {
                path: path.display().to_string(),
            }),
            Err(error) => Err(io_error(&path, error)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    use agent_domain::model::message::ContentBlock;
    use agent_domain::model::tool::{ToolCall, ToolCallId, ToolName, ToolResult};
    use agent_domain::ports::conversation_log::ConversationLog;

    use crate::session::FileConversationLog;

    fn call() -> ToolCall {
        ToolCall::new(
            ToolCallId::new("c1"),
            ToolName::new("read_file").unwrap(),
            serde_json::json!({"path": "a.rs"}),
        )
    }

    /// Writes through the real log, so the two halves cannot drift apart.
    async fn seeded(directory: &Path, session_id: &str, messages: Vec<Message>) {
        FileConversationLog::new(directory)
            .append(session_id, &messages)
            .await
            .unwrap();
    }

    /// Backdates a file by `seconds`, so that ordering and age are decided
    /// rather than raced. Real timestamps are too coarse to separate two
    /// writes, and sleeping long enough to separate them would be a second of
    /// every test run.
    fn age(path: &Path, seconds: u64) {
        let file = std::fs::File::options().write(true).open(path).unwrap();
        file.set_times(
            std::fs::FileTimes::new()
                .set_modified(SystemTime::now() - Duration::from_secs(seconds)),
        )
        .unwrap();
    }

    #[tokio::test]
    async fn a_session_comes_back_as_it_was_written() {
        let directory = tempfile::tempdir().unwrap();
        let call = call();
        seeded(
            directory.path(),
            "s1",
            vec![
                Message::user("build a parser"),
                Message::assistant(vec![ContentBlock::ToolCall(call.clone())]),
                Message::tool_results(vec![ToolResult::ok(&call, "contents")]),
                Message::assistant_text("done"),
            ],
        )
        .await;

        let conversation = FileSessionStore::new(directory.path())
            .load("s1")
            .await
            .unwrap();

        assert_eq!(conversation.len(), 4);
        assert_eq!(conversation.messages()[0].text(), "build a parser");
        assert_eq!(conversation.messages()[2].role, Role::Tool);
        assert_eq!(
            conversation.messages()[3].seq,
            Some(3),
            "a replay renumbers from the start, which is the same numbering it had"
        );
    }

    #[tokio::test]
    async fn a_damaged_line_ends_the_replay_instead_of_being_skipped() {
        // Skipping would leave the tool result with no call to attach to, and
        // the provider rejects that pair outright.
        let directory = tempfile::tempdir().unwrap();
        let call = call();
        seeded(
            directory.path(),
            "s1",
            vec![
                Message::user("build a parser"),
                Message::assistant(vec![ContentBlock::ToolCall(call.clone())]),
            ],
        )
        .await;

        let path = directory.path().join("s1.jsonl");
        let mut text = std::fs::read_to_string(&path).unwrap();
        text.push_str("{\"v\":1,\"role\":\"tool\",\n");
        std::fs::write(&path, text).unwrap();

        let conversation = FileSessionStore::new(directory.path())
            .load("s1")
            .await
            .unwrap();

        assert_eq!(
            conversation.len(),
            1,
            "the unanswered call goes too, or the next request carries it"
        );
        assert_eq!(conversation.messages()[0].text(), "build a parser");
    }

    #[tokio::test]
    async fn a_line_from_a_newer_agent_stops_the_replay() {
        let directory = tempfile::tempdir().unwrap();
        seeded(directory.path(), "s1", vec![Message::user("first")]).await;
        std::fs::write(
            directory.path().join("s1.jsonl"),
            format!(
                "{}{}\n",
                std::fs::read_to_string(directory.path().join("s1.jsonl")).unwrap(),
                serde_json::json!({
                    "v": SCHEMA_VERSION + 1,
                    "role": "user",
                    "content": [{"type": "text", "text": "from the future"}]
                })
            ),
        )
        .unwrap();

        let conversation = FileSessionStore::new(directory.path())
            .load("s1")
            .await
            .unwrap();

        assert_eq!(conversation.len(), 1);
        assert_eq!(conversation.messages()[0].text(), "first");
    }

    #[tokio::test]
    async fn listing_is_newest_first_and_says_what_each_session_was_about() {
        let directory = tempfile::tempdir().unwrap();
        seeded(
            directory.path(),
            "older",
            vec![Message::user("write the parser")],
        )
        .await;
        age(&directory.path().join("older.jsonl"), 60);
        seeded(
            directory.path(),
            "newer",
            vec![
                Message::user("fix the lexer"),
                Message::assistant_text("ok"),
            ],
        )
        .await;

        let sessions = FileSessionStore::new(directory.path())
            .list()
            .await
            .unwrap();

        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[0].id, "newer", "newest first");
        assert_eq!(sessions[0].preview, "fix the lexer");
        assert_eq!(
            sessions[0].bytes,
            std::fs::metadata(directory.path().join("newer.jsonl"))
                .unwrap()
                .len(),
            "the size is what decides whether a session is worth deleting"
        );
        assert_eq!(sessions[1].id, "older");
        assert_eq!(sessions[1].preview, "write the parser");

        assert_eq!(
            FileSessionStore::new(directory.path())
                .latest()
                .await
                .unwrap()
                .as_deref(),
            Some("newer")
        );
    }

    /// The deliberate limit of reading only the front of a record.
    ///
    /// This is the trade that keeps listing proportional to the number of
    /// sessions rather than to their size, and the cost of it is visible here:
    /// an opening request larger than the scan window gets no preview at all.
    #[tokio::test]
    async fn an_opening_request_past_the_scan_window_gets_no_preview() {
        let directory = tempfile::tempdir().unwrap();
        seeded(
            directory.path(),
            "huge",
            vec![
                Message::user("x".repeat(PREVIEW_SCAN_BYTES * 2)),
                Message::user("this one is never reached"),
            ],
        )
        .await;

        let sessions = FileSessionStore::new(directory.path())
            .list()
            .await
            .unwrap();

        assert_eq!(sessions[0].preview, "");
        assert!(
            sessions[0].bytes > PREVIEW_SCAN_BYTES as u64,
            "the size still comes from the metadata, which costs nothing"
        );
    }

    // --- retention ---------------------------------------------------------

    async fn seeded_sessions(directory: &Path, ids: &[(&str, u64)]) {
        for (id, seconds_old) in ids {
            seeded(directory, id, vec![Message::user(*id)]).await;
            age(&directory.join(format!("{id}.jsonl")), *seconds_old);
        }
    }

    async fn remaining(store: &FileSessionStore) -> Vec<String> {
        store
            .list()
            .await
            .unwrap()
            .into_iter()
            .map(|session| session.id)
            .collect()
    }

    #[tokio::test]
    async fn only_the_newest_sessions_are_kept() {
        let directory = tempfile::tempdir().unwrap();
        seeded_sessions(
            directory.path(),
            &[("a", 30), ("b", 20), ("c", 10), ("d", 5)],
        )
        .await;
        let store = FileSessionStore::new(directory.path());

        let removed = store
            .prune(
                &RetentionPolicy {
                    keep: Some(2),
                    max_age: None,
                },
                &[],
            )
            .await
            .unwrap();

        assert_eq!(removed, vec!["b", "a"], "oldest first, and reported");
        assert_eq!(remaining(&store).await, vec!["d", "c"]);
    }

    #[tokio::test]
    async fn sessions_older_than_the_limit_go_however_few_there_are() {
        let directory = tempfile::tempdir().unwrap();
        seeded_sessions(directory.path(), &[("ancient", 10_000), ("fresh", 5)]).await;
        let store = FileSessionStore::new(directory.path());

        let removed = store
            .prune(
                &RetentionPolicy {
                    keep: None,
                    max_age: Some(Duration::from_secs(3_600)),
                },
                &[],
            )
            .await
            .unwrap();

        assert_eq!(removed, vec!["ancient"]);
        assert_eq!(remaining(&store).await, vec!["fresh"]);
    }

    #[tokio::test]
    async fn a_protected_session_survives_both_rules() {
        // The session someone is about to resume by id. It is old enough for
        // either rule to take it, which is exactly why it needs protecting:
        // reaching for an old record by name is the normal reason to have one.
        let directory = tempfile::tempdir().unwrap();
        seeded_sessions(
            directory.path(),
            &[("wanted", 10_000), ("a", 900), ("b", 800), ("c", 5)],
        )
        .await;
        let store = FileSessionStore::new(directory.path());

        store
            .prune(
                &RetentionPolicy {
                    keep: Some(1),
                    max_age: Some(Duration::from_secs(60)),
                },
                &["wanted"],
            )
            .await
            .unwrap();

        let left = remaining(&store).await;
        assert!(left.contains(&"wanted".to_string()), "{left:?}");
        assert!(left.contains(&"c".to_string()), "{left:?}");
        assert_eq!(left.len(), 2, "{left:?}");
    }

    #[tokio::test]
    async fn an_unlimited_policy_touches_nothing() {
        let directory = tempfile::tempdir().unwrap();
        seeded_sessions(directory.path(), &[("a", 10_000_000), ("b", 5)]).await;
        let store = FileSessionStore::new(directory.path());

        assert!(
            store
                .prune(&RetentionPolicy::default(), &[])
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(remaining(&store).await.len(), 2);
    }

    #[tokio::test]
    async fn pruning_an_empty_directory_is_not_an_error() {
        let directory = tempfile::tempdir().unwrap();
        let store = FileSessionStore::new(directory.path().join("never-written"));

        assert!(
            store
                .prune(
                    &RetentionPolicy {
                        keep: Some(1),
                        max_age: None
                    },
                    &[]
                )
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn an_empty_directory_is_not_an_error() {
        let directory = tempfile::tempdir().unwrap();
        let store = FileSessionStore::new(directory.path().join("never-written"));

        assert!(store.list().await.unwrap().is_empty());
        assert_eq!(store.latest().await.unwrap(), None);
        assert!(matches!(
            store.load("nope").await,
            Err(FsError::NotFound { .. })
        ));
    }

    #[tokio::test]
    async fn deleting_removes_the_record() {
        let directory = tempfile::tempdir().unwrap();
        seeded(directory.path(), "s1", vec![Message::user("hello")]).await;
        let store = FileSessionStore::new(directory.path());

        store.delete("s1").await.unwrap();

        assert!(store.list().await.unwrap().is_empty());
        assert!(matches!(
            store.delete("s1").await,
            Err(FsError::NotFound { .. })
        ));
    }

    #[tokio::test]
    async fn a_traversing_id_cannot_reach_outside_the_directory() {
        let parent = tempfile::tempdir().unwrap();
        let directory = parent.path().join("sessions");
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(parent.path().join("secret.jsonl"), "{}\n").unwrap();

        let store = FileSessionStore::new(&directory);

        assert!(matches!(
            store.load("../secret").await,
            Err(FsError::NotFound { .. })
        ));
        assert!(matches!(
            store.delete("../secret").await,
            Err(FsError::NotFound { .. })
        ));
        assert!(
            parent.path().join("secret.jsonl").exists(),
            "a file outside the directory must survive a delete aimed at it"
        );
    }
}
