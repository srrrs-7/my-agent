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
}

impl Session {
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            conversation: Conversation::new(),
            usage: TokenUsage::default(),
        }
    }

    pub fn turns(&self) -> usize {
        self.conversation.len()
    }
}
