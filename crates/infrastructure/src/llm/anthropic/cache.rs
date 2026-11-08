//! Prompt caching.
//!
//! Every turn of the agent loop re-sends the tool definitions, the system
//! prompt and the whole history so far, and Anthropic caches only what it is
//! told to - without markers each turn pays full price for all of it again.
//! Two markers are sent, both governed by [`PromptCache`]:
//!
//! * one on the last block of the request's own system prompt. The API renders
//!   `tools`, then `system`, then `messages`, so this caches the tool
//!   definitions and the system prompt together - the part that stays fixed
//!   for a whole session - and gives it a read point that survives whatever
//!   later happens to the history, compaction and trimming included;
//! * a top-level `cache_control` (automatic caching): the server puts a
//!   breakpoint on the last block and moves it forward as the conversation
//!   grows, so each turn reads everything the previous turn sent.
//!
//! A hit needs the prefix to be byte-identical, so everything in front of a
//! breakpoint must serialise the same way every time. It does: tools arrive
//! sorted by name from the registry, `serde_json` objects (the input schemas)
//! serialise in a fixed key order, and the loop builds the system prompt once
//! per session rather than once per run.
//!
//! A prefix shorter than the model's minimum (512 to 4096 tokens, depending on
//! the model) is silently not cached - no error, just zeros in the cache
//! counts. Neither feature needs a beta header.

use std::str::FromStr;

use agent_domain::model::llm::TaskKind;
use serde::Serialize;

/// How long a cached prompt prefix lives (`AGENT_PROMPT_CACHE`).
///
/// Writing to the cache costs more than plain input (1.25x for five minutes,
/// 2x for an hour) and reading from it far less, and every read restarts the
/// clock. Five minutes therefore suits an agent loop, whose turns follow one
/// another well inside that window; the hour pays off only when a session
/// regularly sits idle between prompts for longer. `Off` is for gateways that
/// reject `cache_control`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PromptCache {
    Off,
    #[default]
    FiveMinutes,
    OneHour,
}

impl PromptCache {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::FiveMinutes => "5m",
            Self::OneHour => "1h",
        }
    }

    /// The marker both breakpoints carry. They must agree: the API rejects a
    /// longer TTL behind a shorter one.
    pub(super) fn marker(self) -> Option<CacheControl> {
        match self {
            Self::Off => None,
            // Five minutes is the API's default, so it is left unsaid.
            Self::FiveMinutes => Some(CacheControl {
                kind: "ephemeral",
                ttl: None,
            }),
            Self::OneHour => Some(CacheControl {
                kind: "ephemeral",
                ttl: Some("1h"),
            }),
        }
    }
}

impl FromStr for PromptCache {
    type Err = String;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "5m" | "5min" | "on" | "true" => Ok(Self::FiveMinutes),
            "1h" | "60m" | "1hour" => Ok(Self::OneHour),
            "off" | "false" | "none" | "0" => Ok(Self::Off),
            other => Err(format!(
                "unknown prompt cache setting `{other}` (expected 5m|1h|off)"
            )),
        }
    }
}

/// Whether a later request will start with everything this one sends.
///
/// Automatic caching writes everything up to the last block that is not
/// cached yet, at a premium over plain input. Each turn of a conversation earns
/// that back on the next turn; a one-off side call never does. Compaction is the case that matters:
/// its transcript is summarised once and never sent again, so marking it would
/// only make the largest request of the session a quarter dearer. Its system
/// prompt is still marked - that part *is* sent again by the next compaction.
pub(super) fn resends_its_tail(kind: TaskKind) -> bool {
    match kind {
        TaskKind::Agentic | TaskKind::Chat => true,
        TaskKind::Summarize | TaskKind::Classify => false,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub(super) struct CacheControl {
    #[serde(rename = "type")]
    kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    ttl: Option<&'static str>,
}
