//! What the next request is made of, for `/context`.
//!
//! Sizes are the same byte proxy the loop budgets with
//! ([`Message::approx_bytes`](agent_domain::model::message::Message::approx_bytes)),
//! not tokens: tokenising belongs to a provider. The provider's own count is
//! here too, as the usage of the newest response, and the two together say
//! both where the bytes are and what they cost.

use agent_domain::model::llm::{ModelId, TokenUsage};
use agent_domain::model::tool::ToolDefinition;

/// Where the bytes of a session's next request would go.
#[derive(Debug, Clone, PartialEq)]
pub struct ContextReport {
    /// The model the next request asks for; `None` leaves it to the provider.
    pub model: Option<ModelId>,
    /// `None` before the session's first turn rendered the system prompt.
    pub system_bytes: Option<usize>,
    pub tool_count: usize,
    /// The tool definitions as advertised: name, description and schema.
    pub tools_bytes: usize,
    /// The whole history, including a plan note that is in it.
    pub history_bytes: usize,
    pub messages: usize,
    /// What the history is fitted to before each request. A plan note is
    /// counted against it, whether it is in the history or only reserved.
    pub history_budget: usize,
    /// The note that would repeat the plan after a shortening; `None` when
    /// there is no unfinished plan to repeat.
    pub plan_note: Option<PlanNote>,
    /// What the newest model response cost, cache reads and writes
    /// included; `None` before the first one.
    pub last_usage: Option<TokenUsage>,
    /// How many times the history was folded into a summary in this process.
    pub compactions: u32,
}

/// The plan note's share of the budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlanNote {
    pub bytes: usize,
    /// A copy is in the history, so `history_bytes` already counts it;
    /// otherwise its bytes are held back from the budget.
    pub in_history: bool,
}

/// One tool definition's size, measured like a message is: the text it
/// carries, not its wire encoding, which differs by provider.
pub(crate) fn definition_bytes(definition: &ToolDefinition) -> usize {
    definition.name.as_str().len()
        + definition.description.len()
        + definition.input_schema.to_string().len()
}
