//! The small closed vocabularies configuration is written in.
//!
//! Each one parses leniently (people write `ollama`, `openai_compatible` and
//! `OpenAI` meaning the same thing) but reports the accepted values on failure,
//! because a typo in a `.env` file is otherwise a five-minute mystery.

use std::str::FromStr;

/// Which wire protocol an endpoint speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderKind {
    /// OpenAI `/chat/completions`. Also covers Ollama, vLLM, LM Studio,
    /// llama.cpp, OpenRouter, Groq, Together, ...
    OpenAiCompatible,
    /// Anthropic `/v1/messages`.
    Anthropic,
}

impl ProviderKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OpenAiCompatible => "openai",
            Self::Anthropic => "anthropic",
        }
    }
}

impl FromStr for ProviderKind {
    type Err = String;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        raw.parse::<ProviderPreset>().map(ProviderPreset::kind)
    }
}

/// What a provider name in the configuration means: a wire protocol *and*
/// where that protocol is usually served.
///
/// [`ProviderKind`] alone cannot carry the second half. `ollama` and `openai`
/// speak the same protocol but live at different addresses, and collapsing
/// the name into the protocol is what used to send `AGENT_PROVIDER=openai`
/// without a base URL to a local Ollama port. Keeping the name lets three
/// variables (provider, key, model) be a complete configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderPreset {
    /// The OpenAI API itself.
    OpenAi,
    /// The Ollama service of this repository's compose file (`make ollama-up`).
    Ollama,
    /// The Anthropic API.
    Anthropic,
}

impl ProviderPreset {
    pub fn kind(self) -> ProviderKind {
        match self {
            Self::OpenAi | Self::Ollama => ProviderKind::OpenAiCompatible,
            Self::Anthropic => ProviderKind::Anthropic,
        }
    }

    /// Endpoint used when no base URL is configured.
    ///
    /// `ollama` is the compose service name, not `localhost`: inside the dev
    /// container `localhost` is the container itself, where nothing listens.
    pub(super) fn default_base_url(self) -> &'static str {
        match self {
            Self::OpenAi => "https://api.openai.com/v1",
            Self::Ollama => "http://ollama:11434/v1",
            Self::Anthropic => "https://api.anthropic.com",
        }
    }

    /// Vendor-conventional variable consulted when no explicit key is given.
    ///
    /// Ollama has none. It shares the OpenAI protocol but not OpenAI's
    /// credentials: falling back to `OPENAI_API_KEY` would send a real OpenAI
    /// key to whatever answers at the Ollama address. A key for Ollama (say,
    /// behind an authenticating proxy) has to be given explicitly.
    pub(super) fn vendor_api_key_env(self) -> Option<&'static str> {
        match self {
            Self::OpenAi => Some("OPENAI_API_KEY"),
            Self::Ollama => None,
            Self::Anthropic => Some("ANTHROPIC_API_KEY"),
        }
    }

    /// Whether a missing key should stop startup rather than surface as a 401
    /// on the first request.
    ///
    /// For `openai` this only holds while the endpoint is the default one: an
    /// explicit base URL means a compatible server (vLLM, LM Studio, ...) that
    /// often takes no key at all. Anthropic always needs one.
    pub(super) fn requires_api_key(self, endpoint_overridden: bool) -> bool {
        match self {
            Self::OpenAi => !endpoint_overridden,
            Self::Ollama => false,
            Self::Anthropic => true,
        }
    }

    /// The tail of the "model is not set" message. Deliberately names no
    /// OpenAI model: an ID guessed here would be wrong sooner or later, and a
    /// wrong example is worse than none.
    pub(super) fn model_hint(self, base_url: &str) -> String {
        match self {
            Self::OpenAi => format!(
                "pick a model served by {base_url} - for the OpenAI API, an OpenAI model ID"
            ),
            Self::Ollama => format!(
                "pick a model served by {base_url}, e.g. `qwen3:8b` \
                 (download it with `make ollama-pull MODEL=qwen3:8b`)"
            ),
            Self::Anthropic => {
                format!("pick a model served by {base_url}, e.g. `claude-sonnet-5-5`")
            }
        }
    }
}

impl FromStr for ProviderPreset {
    type Err = String;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "openai" | "openai-compatible" | "openai_compatible" | "compatible" => Ok(Self::OpenAi),
            "ollama" => Ok(Self::Ollama),
            "anthropic" | "claude" => Ok(Self::Anthropic),
            other => Err(format!(
                "unknown provider `{other}` (expected openai|ollama|anthropic)"
            )),
        }
    }
}

/// How aggressively tool calls are confirmed with the human.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalPolicy {
    /// Never ask. Only sane inside the dev container.
    Auto,
    /// Read-only tools run freely, everything else is confirmed.
    ReadOnlyAuto,
    /// Confirm every call.
    Ask,
}

impl ApprovalPolicy {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::ReadOnlyAuto => "read-only",
            Self::Ask => "ask",
        }
    }
}

impl FromStr for ApprovalPolicy {
    type Err = String;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "auto" | "yes" | "always" => Ok(Self::Auto),
            "read-only" | "readonly" | "read_only" => Ok(Self::ReadOnlyAuto),
            "ask" | "always-ask" | "prompt" => Ok(Self::Ask),
            other => Err(format!(
                "unknown approval policy `{other}` (expected auto|read-only|ask)"
            )),
        }
    }
}

/// Which routing policy sits in front of the configured providers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouterKind {
    /// Always the default provider.
    Static,
    /// `--model cloud/claude-sonnet-5` selects the `cloud` provider.
    ModelPrefix,
}

impl RouterKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Static => "static",
            Self::ModelPrefix => "model-prefix",
        }
    }
}

impl FromStr for RouterKind {
    type Err = String;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "static" | "default" | "single" => Ok(Self::Static),
            "model-prefix" | "model_prefix" | "prefix" => Ok(Self::ModelPrefix),
            other => Err(format!(
                "unknown router `{other}` (expected static|model-prefix)"
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_kind_accepts_common_spellings() {
        for raw in ["openai", "OpenAI", " openai_compatible ", "ollama"] {
            assert_eq!(
                raw.parse::<ProviderKind>().unwrap(),
                ProviderKind::OpenAiCompatible
            );
        }
        assert_eq!(
            "claude".parse::<ProviderKind>().unwrap(),
            ProviderKind::Anthropic
        );
    }

    #[test]
    fn provider_names_keep_where_they_are_served() {
        assert_eq!(
            "OpenAI".parse::<ProviderPreset>().unwrap(),
            ProviderPreset::OpenAi
        );
        assert_eq!(
            "compatible".parse::<ProviderPreset>().unwrap(),
            ProviderPreset::OpenAi
        );
        assert_eq!(
            " ollama ".parse::<ProviderPreset>().unwrap(),
            ProviderPreset::Ollama
        );
        assert_eq!(
            "claude".parse::<ProviderPreset>().unwrap(),
            ProviderPreset::Anthropic
        );
        assert_eq!(
            ProviderPreset::Ollama.kind(),
            ProviderPreset::OpenAi.kind(),
            "the two differ in address, not in protocol"
        );
    }

    #[test]
    fn unknown_values_name_the_alternatives() {
        let error = "gemini".parse::<ProviderKind>().unwrap_err();
        assert!(error.contains("openai"), "{error}");
        assert!(error.contains("anthropic"), "{error}");
        assert!(error.contains("ollama"), "{error}");
    }

    #[test]
    fn approval_policy_parsing() {
        assert_eq!(
            "auto".parse::<ApprovalPolicy>().unwrap(),
            ApprovalPolicy::Auto
        );
        assert_eq!(
            "read-only".parse::<ApprovalPolicy>().unwrap(),
            ApprovalPolicy::ReadOnlyAuto
        );
        assert_eq!(
            "readonly".parse::<ApprovalPolicy>().unwrap(),
            ApprovalPolicy::ReadOnlyAuto
        );
        assert_eq!(
            "ask".parse::<ApprovalPolicy>().unwrap(),
            ApprovalPolicy::Ask
        );
        assert!("sometimes".parse::<ApprovalPolicy>().is_err());
    }

    #[test]
    fn router_parsing() {
        assert_eq!("static".parse::<RouterKind>().unwrap(), RouterKind::Static);
        assert_eq!(
            "model-prefix".parse::<RouterKind>().unwrap(),
            RouterKind::ModelPrefix
        );
        assert!("cheapest".parse::<RouterKind>().is_err());
    }

    #[test]
    fn labels_round_trip_through_parsing() {
        for kind in [ProviderKind::OpenAiCompatible, ProviderKind::Anthropic] {
            assert_eq!(kind.as_str().parse::<ProviderKind>().unwrap(), kind);
        }
        for policy in [
            ApprovalPolicy::Auto,
            ApprovalPolicy::ReadOnlyAuto,
            ApprovalPolicy::Ask,
        ] {
            assert_eq!(policy.as_str().parse::<ApprovalPolicy>().unwrap(), policy);
        }
        for router in [RouterKind::Static, RouterKind::ModelPrefix] {
            assert_eq!(router.as_str().parse::<RouterKind>().unwrap(), router);
        }
    }
}
