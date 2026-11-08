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
    let messages = session.conversation.messages();
    if latest_plan(messages).as_ref() == session.plan() {
        return true;
    }
    let note = reminder.text();
    messages
        .iter()
        .any(|message| message.role == Role::User && message.text() == note)
}
