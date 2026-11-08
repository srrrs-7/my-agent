use agent_domain::model::conversation::Conversation;
use agent_domain::model::llm::{ModelId, TokenUsage};
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
    /// Notes for the model about things the user did between turns (an
    /// `/undo`), put in front of the next user message. Each carries the
    /// number of the user message of the turn it is about, so a rewind past
    /// that turn drops the note along with it.
    ///
    /// In the user message rather than the system prompt, which stays fixed
    /// for the session so the provider's prompt cache keeps matching.
    pub(crate) notes: Vec<(u64, String)>,
    /// The model chosen with `/model`, preferred over the configured one by
    /// every request this session makes. `None` runs on the configuration.
    ///
    /// Set only between turns, through [`Self::switch_model`], so a turn runs
    /// on one model from start to finish. Not carried into a resumed session:
    /// the record says what was said, not which model was listening.
    pub(crate) model_override: Option<ModelId>,
    /// What the newest model response of the agent loop cost - unlike
    /// [`Self::usage`], not a running total. Its input side is how big the
    /// last request was, which is what `/context` wants to show. Summary
    /// requests are left out: they carry a transcript, not this context.
    pub(crate) last_usage: Option<TokenUsage>,
    /// How many times the history was folded into a summary in this process.
    pub(crate) compactions: u32,
}

impl Session {
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            conversation: Conversation::new(),
            usage: TokenUsage::default(),
            system_prompt: None,
            plan: None,
            notes: Vec::new(),
            model_override: None,
            last_usage: None,
            compactions: 0,
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

    /// Takes the conversation back to just before the user message numbered
    /// `seq` (see [`Conversation::rewind_to`]), returning how many messages
    /// went. The plan and any pending note are brought in line with what is
    /// left.
    pub fn rewind_to(&mut self, seq: u64) -> Option<usize> {
        let dropped = self.conversation.rewind_to(seq)?;
        self.plan = latest_plan(self.conversation.messages()).filter(|plan| !plan.is_empty());
        self.notes.retain(|(about, _)| *about < seq);
        Some(dropped)
    }

    pub(crate) fn add_note(&mut self, about: u64, note: String) {
        self.notes.push((about, note));
    }

    /// `input` with every pending note in front of it, emptying the queue.
    pub(crate) fn with_notes(&mut self, input: String) -> String {
        if self.notes.is_empty() {
            return input;
        }
        let mut text = String::new();
        for (_, note) in self.notes.drain(..) {
            text.push_str(&note);
            text.push_str("\n\n");
        }
        text.push_str(&input);
        text
    }

    pub fn turns(&self) -> usize {
        self.conversation.len()
    }

    /// The model's current work plan, if it has one.
    pub fn plan(&self) -> Option<&Plan> {
        self.plan.as_ref()
    }

    /// The model chosen for this session, if one was; see
    /// [`AgentLoop::model`](super::AgentLoop::model) for the one in effect.
    pub fn model_override(&self) -> Option<&ModelId> {
        self.model_override.as_ref()
    }

    /// Runs every later request of this session on `model`, returning how
    /// many opaque blocks were dropped from the history for it.
    ///
    /// Those blocks are the previous model's private state - a thinking
    /// block's signature is checked against the model that wrote it - so they
    /// cannot follow the conversation to another one (see
    /// [`Conversation::drop_opaque`]). The system prompt is left exactly as
    /// it is: the switch already costs the provider's prompt cache, and
    /// rebuilding the prompt would also change what the model is told.
    pub fn switch_model(&mut self, model: ModelId) -> usize {
        self.model_override = Some(model);
        self.conversation.drop_opaque()
    }

    /// What the newest model response cost, or `None` before the first one.
    pub fn last_usage(&self) -> Option<TokenUsage> {
        self.last_usage
    }

    /// How many times the history was folded into a summary so far.
    pub fn compactions(&self) -> u32 {
        self.compactions
    }

    /// The system prompt the session sends, or `None` before its first turn
    /// rendered it.
    pub fn system_prompt(&self) -> Option<&str> {
        self.system_prompt.as_deref()
    }
}
