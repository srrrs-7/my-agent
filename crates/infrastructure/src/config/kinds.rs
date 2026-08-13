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

    pub(super) fn default_base_url(self) -> &'static str {
        match self {
            Self::OpenAiCompatible => "http://localhost:11434/v1",
            Self::Anthropic => "https://api.anthropic.com",
        }
    }

    /// Vendor-conventional variable consulted when no explicit key is given.
    pub(super) fn default_api_key_env(self) -> &'static str {
        match self {
            Self::OpenAiCompatible => "OPENAI_API_KEY",
            Self::Anthropic => "ANTHROPIC_API_KEY",
        }
    }
}

impl FromStr for ProviderKind {
    type Err = String;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "openai" | "openai-compatible" | "openai_compatible" | "ollama" | "compatible" => {
                Ok(Self::OpenAiCompatible)
            }
            "anthropic" | "claude" => Ok(Self::Anthropic),
            other => Err(format!(
                "unknown provider kind `{other}` (expected openai|anthropic)"
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
    fn unknown_values_name_the_alternatives() {
        let error = "gemini".parse::<ProviderKind>().unwrap_err();
        assert!(error.contains("openai"), "{error}");
        assert!(error.contains("anthropic"), "{error}");
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
