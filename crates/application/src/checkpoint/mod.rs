//! Taking back what the agent wrote: `/undo` and `/rewind`.
//!
//! [`CheckpointingTool`] records each file write as it is about to happen -
//! the text before and the text after - into a [`CheckpointJournal`], grouped
//! by the user turn it happened in. Undoing a turn walks its writes newest
//! first and asks [`agent_domain::model::checkpoint::restore`] what each one
//! means for the file as it is *now*: only a file that still holds what the
//! write left is put back. Anything else was changed by somebody since and is
//! reported, never overwritten.
//!
//! What is deliberately not here:
//!
//! * **Commands and remote tools.** A command's effect cannot be read off its
//!   arguments, so it cannot be undone from a record of them. Those calls are
//!   recorded by name only, so the user is told what was *not* taken back.
//! * **Persistence.** The journal lives in memory and dies with the process;
//!   a resumed session starts with nothing to undo. The conversation half of
//!   a rewind is persisted - as a mark in the session log - because the log is
//!   what a resume replays.
//! * **Version control.** Nothing here runs `git`: in a workspace the model
//!   can write to, `.git/config` is a way to run code outside the sandbox.

mod tool;

pub use tool::CheckpointingTool;

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use agent_domain::error::FsError;
use agent_domain::model::checkpoint::{self, Restore};
use agent_domain::model::tool::{Before, PlannedWrite, ToolName};
use agent_domain::model::workspace::WorkspacePath;
use agent_domain::ports::conversation_log::ConversationLog;
use agent_domain::ports::file_system::FileSystem;
use thiserror::Error;
use tracing::warn;

use crate::agent::Session;

/// One thing that happened in a turn and may need taking back.
#[derive(Debug, Clone)]
enum Entry {
    /// A file write, as planned right before it ran.
    ///
    /// Kept even when the call then failed. A failure does not prove nothing
    /// was written - an I/O error part-way through, a timeout that abandoned
    /// the write mid-flight - and the restore rule already does the right
    /// thing either way: a file still as it was is left alone, one that was
    /// half-written holds neither text and is reported rather than touched.
    /// Dropping the record would instead lose exactly the case worth seeing.
    Write(PlannedWrite),
    /// A call whose effects are not recorded, so cannot be undone.
    Untracked(ToolName),
}

#[derive(Debug, Default)]
struct Turn {
    /// The sequence number of the user message that opened the turn. `None`
    /// only for writes recorded before any turn began, which no session does.
    seq: Option<u64>,
    entries: Vec<Entry>,
}

/// The writes of the current session, turn by turn, in memory.
///
/// Concrete rather than a port: it is a use-case detail with one
/// implementation, built on the [`FileSystem`] port like any tool.
pub struct CheckpointJournal {
    file_system: Arc<dyn FileSystem>,
    turns: Mutex<Vec<Turn>>,
}

impl CheckpointJournal {
    pub fn new(file_system: Arc<dyn FileSystem>) -> Self {
        Self {
            file_system,
            turns: Mutex::new(Vec::new()),
        }
    }

    /// Starts a turn opened by the user message numbered `seq`. Called by the
    /// loop as it pushes that message.
    pub fn begin_turn(&self, seq: u64) {
        self.lock().push(Turn {
            seq: Some(seq),
            entries: Vec::new(),
        });
    }

    /// Forgets everything, for a session that starts over.
    pub fn clear(&self) {
        self.lock().clear();
    }

    /// How many turns have been recorded.
    pub fn turns(&self) -> usize {
        self.lock().len()
    }

    fn record(&self, entries: impl IntoIterator<Item = Entry>) {
        let mut turns = self.lock();
        if turns.is_empty() {
            turns.push(Turn::default());
        }
        if let Some(turn) = turns.last_mut() {
            turn.entries.extend(entries);
        }
    }

    pub(crate) fn record_writes(&self, writes: Vec<PlannedWrite>) {
        self.record(writes.into_iter().map(Entry::Write));
    }

    pub(crate) fn record_untracked(&self, tool: ToolName) {
        self.record([Entry::Untracked(tool)]);
    }

    /// Takes back the newest turn that recorded anything, and forgets its
    /// record. `None` when no turn did.
    ///
    /// The turn itself stays, empty, so that [`Self::turn_seq`] still counts
    /// it: `/rewind 1` after `/undo` means the same turn it meant before.
    pub async fn undo_last(&self) -> Option<RestoreReport> {
        let (seq, entries) = {
            let mut turns = self.lock();
            let turn = turns
                .iter_mut()
                .rev()
                .find(|turn| !turn.entries.is_empty())?;
            (turn.seq, std::mem::take(&mut turn.entries))
        };
        let mut report = self.restore(entries).await;
        report.seq = seq;
        Some(report)
    }

    /// The number of the user message that opened the `back`-th newest turn
    /// (`1` is the newest), or `None` if fewer turns were recorded.
    pub fn turn_seq(&self, back: usize) -> Option<u64> {
        let turns = self.lock();
        let index = turns.len().checked_sub(back.max(1))?;
        turns[index].seq
    }

    /// Takes back every turn opened at or after `seq`, newest first, and
    /// forgets them.
    pub async fn rewind_files(&self, seq: u64) -> RestoreReport {
        let entries: Vec<Entry> = {
            let mut turns = self.lock();
            let from = turns
                .iter()
                .position(|turn| turn.seq.is_some_and(|start| start >= seq))
                .unwrap_or(turns.len());
            turns.drain(from..).flat_map(|turn| turn.entries).collect()
        };
        let mut report = self.restore(entries).await;
        report.seq = Some(seq);
        report
    }

    /// Undoes `entries` in reverse: a file written twice has to go back
    /// through its middle state to reach its first one.
    async fn restore(&self, entries: Vec<Entry>) -> RestoreReport {
        let mut report = RestoreReport::default();
        for entry in entries.into_iter().rev() {
            match entry {
                Entry::Untracked(tool) => push_unique(&mut report.not_undone, tool),
                Entry::Write(write) => self.restore_write(write, &mut report).await,
            }
        }
        report
    }

    async fn restore_write(&self, write: PlannedWrite, report: &mut RestoreReport) {
        // The file the write reached, not the name it was asked for by: a
        // link named in the call may point elsewhere by now.
        let now = match self.file_system.read_to_string(&write.target).await {
            Ok(text) => Before::Text(text),
            Err(FsError::NotFound { .. }) => Before::Absent,
            Err(error) => Before::Unreadable(error.to_string()),
        };
        let result = match checkpoint::restore(&write.before, &write.after, &now) {
            Restore::Rewrite(text) => self.file_system.write(&write.target, &text).await,
            Restore::Remove => self.file_system.remove_file(&write.target).await,
            Restore::Unchanged => return,
            Restore::Conflict => {
                push_unique(&mut report.conflicts, write.path);
                return;
            }
            Restore::Unknown(why) => {
                report.unknown.push((write.path, why));
                return;
            }
        };
        match result {
            Err(error) => report.failed.push((write.path, error.to_string())),
            Ok(()) if write.before == Before::Absent => {
                push_unique(&mut report.removed, write.path)
            }
            Ok(()) => push_unique(&mut report.restored, write.path),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Vec<Turn>> {
        // A panic while holding the lock leaves a list that is still a list;
        // refusing every later undo over it would help nobody.
        self.turns.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

fn push_unique<T: PartialEq>(list: &mut Vec<T>, item: T) {
    if !list.contains(&item) {
        list.push(item);
    }
}

/// What taking back one or more turns did to the files.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RestoreReport {
    /// The user message the earliest turn taken back opened with.
    pub seq: Option<u64>,
    /// Put back to their earlier text.
    pub restored: Vec<WorkspacePath>,
    /// Created by the agent and deleted again.
    pub removed: Vec<WorkspacePath>,
    /// Changed since the agent wrote them, so left as they are.
    pub conflicts: Vec<WorkspacePath>,
    /// Their earlier text was never known (too large, not text), so there is
    /// nothing to put back. With the reason.
    pub unknown: Vec<(WorkspacePath, String)>,
    /// Restoring them was attempted and failed. With the error.
    pub failed: Vec<(WorkspacePath, String)>,
    /// Calls in those turns whose effects are not recorded and were not
    /// undone (commands, remote tools).
    pub not_undone: Vec<ToolName>,
}

impl RestoreReport {
    /// Whether any file is now different from what the model last saw.
    pub fn changed_files(&self) -> bool {
        !self.restored.is_empty() || !self.removed.is_empty()
    }

    /// Whether there is nothing at all to report.
    pub fn is_empty(&self) -> bool {
        !self.changed_files()
            && self.conflicts.is_empty()
            && self.unknown.is_empty()
            && self.failed.is_empty()
            && self.not_undone.is_empty()
    }

    /// The note the model is given ahead of the user's next message after an
    /// `/undo`, or `None` when no file changed.
    ///
    /// Needed because `/undo` leaves the conversation as it was: the model
    /// still sees its own writes and their successful results, and would
    /// otherwise carry on as if the files held what it wrote.
    pub fn note_for_model(&self) -> Option<String> {
        if !self.changed_files() {
            return None;
        }
        let mut note = String::from(
            "[Agent note - this is not a message from the user]\n\
             The user ran /undo. Changes you made to files in an earlier turn were taken back, so \
             those files no longer contain what your tool calls above wrote:\n",
        );
        let mut line = |label: &str, paths: &[WorkspacePath]| {
            for path in paths {
                note.push_str(&format!("- {label}: {path}\n"));
            }
        };
        line("restored to its earlier content", &self.restored);
        line("deleted (it did not exist before)", &self.removed);
        line(
            "left as it is (changed since, so not undone)",
            &self.conflicts,
        );
        note.push_str("Read a file again before relying on its content.");
        Some(note)
    }
}

/// Why `/undo` or `/rewind` did nothing.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum CheckpointError {
    #[error("checkpoints are not enabled in this session")]
    Disabled,
    #[error("nothing to undo: no file was written in this session")]
    NothingToUndo,
    #[error("only {recorded} turn(s) were recorded in this session; nothing was changed")]
    NotThatManyTurns { recorded: usize },
    #[error(
        "that turn has been folded into a summary of the conversation and cannot be rewound to; \
         nothing was changed"
    )]
    Compacted,
}

/// What a rewind did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RewindReport {
    pub files: RestoreReport,
    /// Messages removed from the conversation.
    pub dropped_messages: usize,
    /// Set when the mark could not be written to the session log, so a
    /// resume would bring the removed messages back.
    pub log_error: Option<String>,
}

/// `/undo`: takes back the files of the newest turn that wrote any and tells
/// the model so on its next turn. The conversation is left as it is.
pub(crate) async fn undo(
    journal: &CheckpointJournal,
    session: &mut Session,
) -> Result<RestoreReport, CheckpointError> {
    let report = journal
        .undo_last()
        .await
        .ok_or(CheckpointError::NothingToUndo)?;
    if let (Some(note), Some(seq)) = (report.note_for_model(), report.seq) {
        session.add_note(seq, note);
    }
    Ok(report)
}

/// `/rewind`: takes the files and the conversation back to before the
/// `turns`-th newest turn.
///
/// The conversation is cut first, because it is the half that can refuse: a
/// turn already folded into a summary cannot be cut out of it, and finding
/// that out after the files were put back would leave the two disagreeing.
pub(crate) async fn rewind(
    journal: &CheckpointJournal,
    log: &dyn ConversationLog,
    session: &mut Session,
    turns: usize,
) -> Result<RewindReport, CheckpointError> {
    let seq = journal
        .turn_seq(turns)
        .ok_or(CheckpointError::NotThatManyTurns {
            recorded: journal.turns(),
        })?;
    let dropped_messages = session.rewind_to(seq).ok_or(CheckpointError::Compacted)?;
    let files = journal.rewind_files(seq).await;
    let log_error = match log.rewind(&session.id, seq).await {
        Ok(()) => None,
        Err(error) => {
            warn!(%error, session = %session.id, "cannot record the rewind in the conversation log");
            Some(error.to_string())
        }
    };
    Ok(RewindReport {
        files,
        dropped_messages,
        log_error,
    })
}
