use agent_domain::model::llm::{GenerationParams, ModelId};

/// Everything that shapes one run of the loop.
#[derive(Debug, Clone, PartialEq)]
pub struct AgentLoopConfig {
    /// Model to request. `None` defers to the provider's own default, which is
    /// also what a router wants to see when it picks the model itself.
    pub model: Option<ModelId>,
    pub params: GenerationParams,

    /// Hard stop on the number of model round-trips. The single most important
    /// safety valve of an agent loop: without it a model that keeps calling
    /// tools burns tokens forever.
    pub max_iterations: u32,

    /// Tool output longer than this is truncated before it enters the history.
    /// One `read_file` on a large file must not evict the whole conversation.
    pub max_tool_output_bytes: usize,

    /// Approximate size budget for the message history.
    pub max_history_bytes: usize,

    /// Messages at the tail that trimming may never drop.
    pub keep_recent_messages: usize,

    /// Fold the oldest turns into an LLM-written summary before falling back to
    /// dropping them. Costs one extra request, and only when the history would
    /// otherwise lose content - see [`super::compaction`].
    pub compact: bool,

    /// Messages left verbatim after a compaction. Larger than
    /// [`Self::keep_recent_messages`] on purpose: that one is a floor trimming
    /// must not cross, this one is how much recent conversation is worth
    /// keeping in its original words once the rest is a summary.
    pub compact_keep_recent: usize,

    /// Run read-only tool calls of the same turn concurrently. Mutating calls
    /// always run sequentially, in the order the model asked for them.
    pub parallel_read_only_tools: bool,

    /// Ask the provider to stream and surface prose incrementally. Providers
    /// without streaming fall back transparently, so this stays `true` unless
    /// a server misbehaves on `stream: true` (the `AGENT_STREAM=false` escape
    /// hatch). `false` also restores the retry semantics of a whole-response
    /// call: a mid-stream failure cannot be retried, a whole request can.
    pub stream: bool,
}

impl Default for AgentLoopConfig {
    fn default() -> Self {
        Self {
            model: None,
            params: GenerationParams {
                temperature: Some(0.2),
                max_tokens: Some(4096),
                top_p: None,
                stop_sequences: Vec::new(),
            },
            max_iterations: 25,
            max_tool_output_bytes: 32 * 1024,
            max_history_bytes: 256 * 1024,
            keep_recent_messages: 6,
            compact: true,
            compact_keep_recent: 12,
            parallel_read_only_tools: true,
            stream: true,
        }
    }
}
