use agent_domain::model::conversation::Conversation;
use agent_domain::model::llm::TokenUsage;
use agent_domain::model::plan::Plan;

use crate::tools::plan::latest_plan;

/// One continuous conversation with the agent.
///
/// The id is supplied by the caller rather than generated here, so this crate
/// needs neither a clock nor a random source.
#[derive(Debug, Clone, Default)]
pub struct Session {
    pub id: String,
    pub conversation: Conversation,
    pub usage: TokenUsage,
    /// The system prompt, rendered on the session's first run and sent
    /// unchanged by every run after it.
    ///
    /// Rendering it again would re-read the workspace - the very thing the
    /// agent has been changing. A file written in one turn would appear in the
    /// directory overview of the next, the prompt's bytes would move, and the
    /// provider's prompt cache, which matches the request prefix byte for byte
    /// starting with the tools and this prompt, would miss on the whole
    /// conversation behind it. The model loses nothing by the overview going
    /// stale: the history already says what it wrote. The date can go stale
    /// too, in a session that runs past midnight - a smaller loss than paying
    /// full price for the whole history on every turn. A new session
    /// (`/reset`, or a resumed one) renders a fresh prompt.
    pub(crate) system_prompt: Option<String>,
    /// The model's current work plan, `None` when it has none.
    ///
    /// A cache, not the source of truth: the plan *is* the last successful
    /// `todo_write` in the conversation (see [`crate::tools::plan`]). It is
    /// kept because the conversation gets shortened, and once the call that
    /// wrote the plan is folded into a summary or trimmed away, this is the
    /// only place the loop can still find it. Refreshed by the loop from each
    /// turn it appends, and rebuilt from the whole record by
    /// [`Self::resumed`].
    pub(crate) plan: Option<Plan>,
}

impl Session {
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            conversation: Conversation::new(),
            usage: TokenUsage::default(),
            system_prompt: None,
            plan: None,
        }
    }

    /// A session carrying on from a recorded conversation, with the plan the
    /// record last wrote.
    ///
    /// The record is the unshortened history, so the projection sees every
    /// `todo_write` the model ever made - which is why the plan needs no file
    /// of its own.
    pub fn resumed(id: impl Into<String>, conversation: Conversation) -> Self {
        let plan = latest_plan(conversation.messages()).filter(|plan| !plan.is_empty());
        Self {
            conversation,
            plan,
            ..Self::new(id)
        }
    }

    pub fn turns(&self) -> usize {
        self.conversation.len()
    }

    /// The model's current work plan, if it has one.
    pub fn plan(&self) -> Option<&Plan> {
        self.plan.as_ref()
    }
}
