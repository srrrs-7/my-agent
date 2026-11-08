use agent_domain::model::conversation::Conversation;
use agent_domain::model::llm::TokenUsage;

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
}

impl Session {
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            conversation: Conversation::new(),
            usage: TokenUsage::default(),
            system_prompt: None,
        }
    }

    pub fn turns(&self) -> usize {
        self.conversation.len()
    }
}
