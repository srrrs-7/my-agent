//! Keeping the model's work plan in front of it.
//!
//! Normally nothing needs doing: the `todo_write` result in the history *is*
//! the plan, and the model reads it there. That stops being true the moment
//! the history is shortened - a compaction folds the call into a summary, a
//! trim deletes it - and a model that loses its plan mid-task re-plans from
//! whatever it can still see.
//!
//! So right after a shortening, and only then, the plan is put back as one
//! appended note - unless the model can still see it, either because the
//! `todo_write` that wrote it survived or because an earlier note for the same
//! plan did. The second case is not hypothetical: when the messages trimming
//! may not drop already exceed the budget, every iteration trims a little more,
//! and without the check every one of them would add another copy. Where it goes is chosen around the prompt cache: the system
//! prompt and the tool list are frozen for the session and never carry the
//! plan, and the note is appended rather than kept at a fixed position, so the
//! request prefix the provider has cached is never rewritten for it. The cache
//! is already lost at that point anyway - the shortening rewrote the front of
//! the history - so the note costs nothing extra.

use agent_domain::model::message::{Message, Role};
use agent_domain::model::plan::Plan;

use super::session::Session;
use crate::tools::plan::{latest_plan, render_checklist};

/// Brings the session's cached plan up to date with the newest `appended`
/// messages, returning the new plan when it changed.
///
/// Only the newest messages are read: everything older has already been seen,
/// and some of it may have been shortened away. A cleared plan comes back as an
/// empty one so the caller can report it.
pub(crate) fn refresh(session: &mut Session, appended: usize) -> Option<Plan> {
    let messages = session.conversation.messages();
    let written = latest_plan(&messages[messages.len().saturating_sub(appended)..])?;

    let changed = match &session.plan {
        Some(current) => *current != written,
        None => !written.is_empty(),
    };
    if !changed {
        return None;
    }
    session.plan = (!written.is_empty()).then(|| written.clone());
    Some(written)
}

/// The history budget with room kept for the plan note, and what is needed
/// to decide afterwards whether the note has to be appended.
pub(crate) struct Reservation {
    /// What the history is shortened to before the note is appended.
    pub budget: usize,
    max_bytes: usize,
    reminder: Option<Message>,
    /// A copy of `reminder` is in the history, so `budget` counts it there
    /// rather than reserving room for another.
    noted: bool,
}

/// The note to append after a shortening.
pub(crate) struct Restore {
    pub note: Message,
    /// `Some` when the history has to be trimmed to this budget first: the
    /// copy the reservation counted is gone, and nothing was reserved for its
    /// replacement.
    pub make_room: Option<usize>,
}

/// Reserves room in `max_bytes` for the note that may have to be appended -
/// unless a copy is already in the history, where its bytes are counted once
/// already. Should the shortening remove that copy, [`Reservation::restore`]
/// asks for room to be made before the new one is appended.
pub(crate) fn reserve(max_bytes: usize, session: &Session) -> Reservation {
    let reminder = reminder(session.plan());
    let noted = reminder
        .as_ref()
        .is_some_and(|note| already_noted(session, note));
    let budget = match &reminder {
        Some(note) if !noted => max_bytes.saturating_sub(note.approx_bytes()),
        _ => max_bytes,
    };
    Reservation {
        budget,
        max_bytes,
        reminder,
        noted,
    }
}

impl Reservation {
    /// Called once the history was shortened: the note to append, or `None`
    /// when there is no unfinished work or the model can still see the plan.
    pub(crate) fn restore(self, session: &Session) -> Option<Restore> {
        let note = self.reminder?;
        if still_visible(session, &note) {
            return None;
        }
        let make_room = self
            .noted
            .then(|| self.max_bytes.saturating_sub(note.approx_bytes()));
        Some(Restore { note, make_room })
    }
}

/// The note that puts `plan` back in front of the model after a shortening, or
/// `None` when there is nothing left to do and so nothing worth the bytes.
///
/// A user-role message because it is the one role every provider accepts in
/// that position; the text says plainly that it is not the user speaking, and
/// that a later plan or a newer instruction outranks it.
pub(crate) fn reminder(plan: Option<&Plan>) -> Option<Message> {
    let plan = plan.filter(|plan| plan.has_unfinished())?;
    Some(Message::user(format!(
        "[Agent note - this is not a message from the user]\n\
         The conversation above was shortened to fit the context window. This is the work plan \
         you last wrote with `todo_write`, repeated so it is not lost:\n\n\
         {}\n\
         Carry on from it. If you write a newer plan with `todo_write`, or the user's most recent \
         message asks for something different, that takes precedence over this note.",
        render_checklist(plan)
    )))
}

/// Whether the model can already see the session's current plan without
/// `reminder`: the call that wrote it is still in the history, or an earlier
/// copy of the same note is.
pub(crate) fn still_visible(session: &Session, reminder: &Message) -> bool {
    latest_plan(session.conversation.messages()).as_ref() == session.plan()
        || already_noted(session, reminder)
}

/// Whether a copy of `reminder` is in the history.
pub(crate) fn already_noted(session: &Session, reminder: &Message) -> bool {
    let note = reminder.text();
    session
        .conversation
        .messages()
        .iter()
        .any(|message| message.role == Role::User && message.text() == note)
}
