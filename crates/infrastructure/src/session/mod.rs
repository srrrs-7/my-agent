//! What a session leaves behind on disk.
//!
//! One directory of `<session-id>.jsonl` files. [`log`] appends to them as the
//! conversation happens; [`store`] reads them back to resume a session or to
//! list what is there.
//!
//! **One file, not two.** Resuming replays the log that was already being
//! written rather than a separate snapshot of the history: two files that are
//! meant to say the same thing eventually disagree - a crash between the two
//! writes is all it takes - and at that point nothing can tell which of them
//! is right.

pub mod log;
pub mod store;

pub use log::FileConversationLog;
pub use store::{FileSessionStore, RetentionPolicy};

use std::path::{Path, PathBuf};

/// Bumped when the shape of a line changes in a way a reader must notice.
///
/// A reader that meets a higher version stops there rather than guessing: the
/// lines it already understood are worth keeping, the ones it does not are not
/// worth inventing.
const SCHEMA_VERSION: u32 = 1;

/// Longest file stem a session id may turn into.
const MAX_STEM_BYTES: usize = 128;

fn path_for(directory: &Path, session_id: &str) -> PathBuf {
    directory.join(format!("{}.jsonl", file_stem(session_id)))
}

/// Turns a session id into a file name that cannot escape the directory.
///
/// The id arrives as a plain string. Today the CLI generates a UUID, but the
/// ports promise nothing, and an id built from anything a user typed could be
/// `../../.ssh/authorized_keys`. Only ASCII alphanumerics, `-` and `_` survive;
/// everything else - including every `.`, which is what makes `..` impossible
/// rather than merely unlikely - becomes `_`.
///
/// Substituting rather than rejecting because a broken id must not be able to
/// turn logging off: a session that lands in a strangely-named file is
/// recoverable, one that was never written is not.
///
/// Idempotent, which is what lets a listing hand its ids straight back to
/// `load` and `delete`: the stem of a stem is the same stem.
fn file_stem(session_id: &str) -> String {
    let cleaned: String = session_id
        .chars()
        .filter(|character| !character.is_whitespace())
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '-' || character == '_' {
                character
            } else {
                '_'
            }
        })
        .take(MAX_STEM_BYTES)
        .collect();

    if cleaned.is_empty() {
        "session".to_string()
    } else {
        cleaned
    }
}

fn io_error(path: &Path, error: std::io::Error) -> agent_domain::error::FsError {
    agent_domain::error::FsError::Io {
        path: path.display().to_string(),
        message: error.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_id_with_nothing_usable_left_still_names_a_file() {
        assert_eq!(file_stem(""), "session");
        assert_eq!(file_stem("   "), "session");
        assert_eq!(file_stem(".."), "__");
        assert_eq!(file_stem("a/b"), "a_b");
    }

    #[test]
    fn cleaning_an_id_twice_changes_nothing() {
        // A listing reports stems, and those stems are handed back to `load`
        // and `delete`. If cleaning were not idempotent they would miss.
        for id in ["../../escaped", "plain-uuid-1234", "réunion", ""] {
            let once = file_stem(id);
            assert_eq!(file_stem(&once), once, "{id}");
        }
    }
}
