//! What undoing one recorded write means, decided from the file as it is now.
//!
//! A write is recorded as the text before and after it. Putting the file back
//! is only safe when it still holds what the write left there: anything else
//! means somebody - the user in an editor, a command the model ran, a later
//! write the record does not know about - changed it since, and writing the
//! old text over that would destroy work nobody asked to lose. So the decision
//! is made by comparing contents, never by assuming the record is the last
//! word on the file.
//!
//! Pure, so the rule can be read and tested without a filesystem; carrying it
//! out is the application's job.

use super::tool::Before;

/// What to do with one file to undo one write to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Restore {
    /// The file still holds what the write left: put this text back.
    Rewrite(String),
    /// The write created the file and it is unchanged since: delete it.
    Remove,
    /// The file is already as it was before the write - the write never
    /// happened, or something already put it back. Nothing to do.
    Unchanged,
    /// The file holds something the write did not leave there. Left alone.
    Conflict,
    /// What the file held before is not known, so there is nothing to put
    /// back. Carries why it was not known.
    Unknown(String),
}

/// Decides how to undo a write that turned `before` into `after`, given the
/// file as it is `now` (in the same three cases a write is planned against).
///
/// `now` matching `before` is checked first: when a write changed nothing,
/// both rules apply, and the one that touches nothing is the one to take.
pub fn restore(before: &Before, after: &str, now: &Before) -> Restore {
    let text = match before {
        Before::Unreadable(why) => return Restore::Unknown(why.clone()),
        Before::Absent => None,
        Before::Text(text) => Some(text.as_str()),
    };

    match (now, text) {
        (Before::Absent, None) => Restore::Unchanged,
        (Before::Text(current), Some(original)) if current == original => Restore::Unchanged,
        (Before::Text(current), None) if current == after => Restore::Remove,
        (Before::Text(current), Some(original)) if current == after => {
            Restore::Rewrite(original.to_string())
        }
        // Gone when it should be there, unreadable, or simply different: the
        // record cannot vouch for what is there now.
        _ => Restore::Conflict,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(value: &str) -> Before {
        Before::Text(value.into())
    }

    #[test]
    fn a_file_still_holding_what_the_write_left_gets_its_old_text_back() {
        assert_eq!(
            restore(&text("old"), "new", &text("new")),
            Restore::Rewrite("old".into())
        );
    }

    #[test]
    fn a_file_already_back_to_its_old_text_is_left_alone() {
        assert_eq!(
            restore(&text("old"), "new", &text("old")),
            Restore::Unchanged
        );
        assert_eq!(
            restore(&Before::Absent, "new", &Before::Absent),
            Restore::Unchanged,
            "a created file that is gone again needs nothing"
        );
    }

    #[test]
    fn a_file_changed_since_the_write_is_a_conflict_and_never_overwritten() {
        assert_eq!(
            restore(&text("old"), "new", &text("edited by hand")),
            Restore::Conflict
        );
        assert_eq!(
            restore(&text("old"), "new", &Before::Absent),
            Restore::Conflict,
            "a file deleted since is not recreated"
        );
        assert_eq!(
            restore(&text("old"), "new", &Before::Unreadable("too large".into())),
            Restore::Conflict,
            "a file that cannot be read now cannot be shown to be unchanged"
        );
    }

    #[test]
    fn a_file_the_write_created_is_deleted_when_it_is_unchanged() {
        assert_eq!(
            restore(&Before::Absent, "new", &text("new")),
            Restore::Remove
        );
        assert_eq!(
            restore(&Before::Absent, "new", &text("new, then edited")),
            Restore::Conflict,
            "a created file that was edited since is kept"
        );
    }

    #[test]
    fn a_file_whose_old_text_was_unknown_cannot_be_put_back() {
        assert_eq!(
            restore(&Before::Unreadable("not UTF-8".into()), "new", &text("new")),
            Restore::Unknown("not UTF-8".into())
        );
    }
}
