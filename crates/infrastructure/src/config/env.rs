//! Reading configuration out of an environment.
//!
//! The environment is reached through [`EnvSource`] rather than through
//! `std::env::var` directly. That indirection buys real testability: the
//! process environment is global mutable state, so tests that set variables
//! cannot run in parallel and leak into each other. With a source, every rule
//! below is exercised against an in-memory map instead.

use std::collections::BTreeMap;
use std::str::FromStr;

use agent_domain::model::llm::{ModelId, ProviderId};

use super::kinds::ProviderPreset;
use super::{ConfigError, ProviderSettings};

/// Read-only view of a set of environment variables.
pub trait EnvSource {
    fn get(&self, key: &str) -> Option<String>;
}

/// The real process environment.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemEnv;

impl EnvSource for SystemEnv {
    fn get(&self, key: &str) -> Option<String> {
        std::env::var(key).ok()
    }
}

impl SystemEnv {
    /// Every variable of the process environment, for
    /// [`super::Settings::secrets`]. Entries that are not valid UTF-8 are
    /// skipped rather than panicking as `std::env::vars` would; such a value
    /// cannot be matched in text output anyway.
    pub fn vars(&self) -> Vec<(String, String)> {
        std::env::vars_os()
            .filter_map(|(name, value)| Some((name.into_string().ok()?, value.into_string().ok()?)))
            .collect()
    }
}

/// In-memory environment for tests and for embedding the agent in a host that
/// keeps its configuration somewhere other than the process environment.
#[derive(Debug, Clone, Default)]
pub struct MapEnv(BTreeMap<String, String>);

impl MapEnv {
    pub fn new(pairs: &[(&str, &str)]) -> Self {
        Self(
            pairs
                .iter()
                .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
                .collect(),
        )
    }

    pub fn set(&mut self, key: &str, value: &str) -> &mut Self {
        self.0.insert(key.to_string(), value.to_string());
        self
    }
}

impl EnvSource for MapEnv {
    fn get(&self, key: &str) -> Option<String> {
        self.0.get(key).cloned()
    }
}

/// Typed accessors over an [`EnvSource`].
pub(super) struct Reader<'a> {
    source: &'a dyn EnvSource,
}

impl<'a> Reader<'a> {
    pub(super) fn new(source: &'a dyn EnvSource) -> Self {
        Self { source }
    }

    /// Trimmed value, treating an empty string as absent - otherwise a commented
    /// out `.env` line like `AGENT_API_KEY=` would override a real key.
    pub(super) fn string(&self, key: &str) -> Option<String> {
        self.source
            .get(key)
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
    }

    pub(super) fn parsed<T>(&self, key: &str, fallback: T) -> Result<T, ConfigError>
    where
        T: FromStr,
        T::Err: std::fmt::Display,
    {
        match self.string(key) {
            None => Ok(fallback),
            Some(raw) => raw
                .parse()
                .map_err(|error: T::Err| ConfigError::invalid(key, raw, error.to_string())),
        }
    }

    /// Comma-separated list, empty entries dropped. Absent and empty both read
    /// as "no entries", which every caller treats as the closed default.
    pub(super) fn list(&self, key: &str) -> Vec<String> {
        self.string(key)
            .map(|raw| {
                raw.split(',')
                    .map(str::trim)
                    .filter(|entry| !entry.is_empty())
                    .map(String::from)
                    .collect()
            })
            .unwrap_or_default()
    }

    pub(super) fn optional<T>(&self, key: &str) -> Result<Option<T>, ConfigError>
    where
        T: FromStr,
        T::Err: std::fmt::Display,
    {
        match self.string(key) {
            None => Ok(None),
            Some(raw) => raw
                .parse()
                .map(Some)
                .map_err(|error: T::Err| ConfigError::invalid(key, raw, error.to_string())),
        }
    }
}

/// Variable names for one provider block.
struct Keys {
    kind: String,
    base_url: String,
    api_key: String,
    model: String,
    max_tokens_field: String,
}

impl Keys {
    /// The single-provider form: `AGENT_PROVIDER`, `AGENT_MODEL`, ...
    fn unaliased() -> Self {
        Self {
            kind: "AGENT_PROVIDER".into(),
            base_url: "AGENT_BASE_URL".into(),
            api_key: "AGENT_API_KEY".into(),
            model: "AGENT_MODEL".into(),
            max_tokens_field: "AGENT_OPENAI_MAX_TOKENS_FIELD".into(),
        }
    }

    /// The multi-provider form: `AGENT_PROVIDER_<ALIAS>_MODEL`, ...
    fn for_alias(alias: &str) -> Self {
        let upper: String = alias
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() {
                    c.to_ascii_uppercase()
                } else {
                    '_'
                }
            })
            .collect();
        Self {
            kind: format!("AGENT_PROVIDER_{upper}_KIND"),
            base_url: format!("AGENT_PROVIDER_{upper}_BASE_URL"),
            api_key: format!("AGENT_PROVIDER_{upper}_API_KEY"),
            model: format!("AGENT_PROVIDER_{upper}_MODEL"),
            max_tokens_field: format!("AGENT_PROVIDER_{upper}_MAX_TOKENS_FIELD"),
        }
    }
}

/// Reads one provider block. `alias = None` reads the single-provider form.
pub(super) fn provider_from_env(
    reader: &Reader<'_>,
    alias: Option<&str>,
) -> Result<ProviderSettings, ConfigError> {
    let (id, keys) = match alias {
        None => (ProviderId::new("default"), Keys::unaliased()),
        Some(alias) => (ProviderId::new(alias), Keys::for_alias(alias)),
    };

    // Required, with no default. Each preset sends the conversation to a
    // different place - a local container or a vendor's API - and that choice
    // is too consequential to make on the user's behalf.
    let preset: ProviderPreset = reader.optional(&keys.kind)?.ok_or_else(|| {
        ConfigError::missing(
            &keys.kind,
            format!(
                "set it to `anthropic | openai | ollama`, e.g. `{}=anthropic`",
                keys.kind
            ),
        )
    })?;
    let kind = preset.kind();

    let explicit_base_url = reader.string(&keys.base_url);
    let endpoint_overridden = explicit_base_url.is_some();
    let base_url = explicit_base_url
        .unwrap_or_else(|| preset.default_base_url().to_string())
        .trim_end_matches('/')
        .to_string();

    let api_key = reader.string(&keys.api_key).or_else(|| {
        preset
            .vendor_api_key_env()
            .and_then(|key| reader.string(key))
    });

    let model = reader
        .string(&keys.model)
        .map(ModelId::new)
        .ok_or_else(|| ConfigError::missing(&keys.model, preset.model_hint(&base_url)))?;

    if api_key.is_none() && preset.requires_api_key(endpoint_overridden) {
        // Only presets that require a key reach here, and each of those has a
        // vendor variable; the empty fallback keeps the hint total regardless.
        let vendor_key = preset.vendor_api_key_env().unwrap_or_default();
        let hint = match preset {
            ProviderPreset::Anthropic => {
                format!("the Anthropic API requires an API key (`{vendor_key}` is also read)")
            }
            _ => format!(
                "{base_url} requires an API key (`{vendor_key}` is also read). For a compatible \
                 server that takes none, set `{}` to its address; for a local Ollama, \
                 set `{}=ollama`",
                keys.base_url, keys.kind
            ),
        };
        return Err(ConfigError::missing(&keys.api_key, hint));
    }

    Ok(ProviderSettings {
        id,
        kind,
        base_url,
        api_key,
        model,
        max_tokens_field: reader
            .string(&keys.max_tokens_field)
            .unwrap_or_else(|| "max_tokens".to_string()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::kinds::ProviderKind;

    fn read(pairs: &[(&str, &str)]) -> MapEnv {
        MapEnv::new(pairs)
    }

    #[test]
    fn empty_values_are_treated_as_absent() {
        let env = read(&[("A", "   "), ("B", "x")]);
        let reader = Reader::new(&env);
        assert_eq!(reader.string("A"), None);
        assert_eq!(reader.string("B").as_deref(), Some("x"));
        assert_eq!(reader.string("MISSING"), None);
    }

    #[test]
    fn parsed_falls_back_and_reports_bad_values() {
        let env = read(&[("N", "12"), ("BAD", "not-a-number")]);
        let reader = Reader::new(&env);

        assert_eq!(reader.parsed::<u32>("N", 5).unwrap(), 12);
        assert_eq!(reader.parsed::<u32>("MISSING", 5).unwrap(), 5);

        let error = reader.parsed::<u32>("BAD", 5).unwrap_err();
        assert!(error.to_string().contains("BAD"), "{error}");
        assert!(error.to_string().contains("not-a-number"), "{error}");
    }

    #[test]
    fn single_provider_uses_the_unaliased_keys() {
        let env = read(&[("AGENT_PROVIDER", "ollama"), ("AGENT_MODEL", "qwen3:8b")]);
        let provider = provider_from_env(&Reader::new(&env), None).unwrap();

        assert_eq!(provider.id.as_str(), "default");
        assert_eq!(provider.kind, ProviderKind::OpenAiCompatible);
        assert_eq!(provider.model.as_str(), "qwen3:8b");
        assert_eq!(
            provider.base_url, "http://ollama:11434/v1",
            "the compose Ollama service by default"
        );
        assert_eq!(provider.max_tokens_field, "max_tokens");
    }

    #[test]
    fn aliases_map_to_prefixed_keys_and_are_case_insensitive() {
        let env = read(&[
            ("AGENT_PROVIDER_MY_CLOUD_KIND", "anthropic"),
            ("AGENT_PROVIDER_MY_CLOUD_MODEL", "claude-sonnet-5"),
            ("AGENT_PROVIDER_MY_CLOUD_API_KEY", "sk-ant-x"),
        ]);
        let provider = provider_from_env(&Reader::new(&env), Some("my-cloud")).unwrap();

        assert_eq!(provider.id.as_str(), "my-cloud");
        assert_eq!(provider.kind, ProviderKind::Anthropic);
        assert_eq!(provider.base_url, "https://api.anthropic.com");
    }

    #[test]
    fn trailing_slashes_are_stripped_from_the_base_url() {
        let env = read(&[
            ("AGENT_PROVIDER", "ollama"),
            ("AGENT_MODEL", "m"),
            ("AGENT_BASE_URL", "http://host:1234/v1/"),
        ]);
        let provider = provider_from_env(&Reader::new(&env), None).unwrap();
        assert_eq!(provider.base_url, "http://host:1234/v1");
    }

    #[test]
    fn falls_back_to_the_vendor_api_key_variable() {
        let env = read(&[
            ("AGENT_PROVIDER", "openai"),
            ("AGENT_MODEL", "gpt-4.1"),
            ("OPENAI_API_KEY", "sk-fallback"),
        ]);
        let provider = provider_from_env(&Reader::new(&env), None).unwrap();
        assert_eq!(provider.api_key.as_deref(), Some("sk-fallback"));
    }

    #[test]
    fn an_explicit_key_wins_over_the_vendor_variable() {
        let env = read(&[
            ("AGENT_PROVIDER", "openai"),
            ("AGENT_MODEL", "gpt-4.1"),
            ("AGENT_API_KEY", "sk-explicit"),
            ("OPENAI_API_KEY", "sk-fallback"),
        ]);
        let provider = provider_from_env(&Reader::new(&env), None).unwrap();
        assert_eq!(provider.api_key.as_deref(), Some("sk-explicit"));
    }

    #[test]
    fn a_missing_model_is_reported_with_a_hint() {
        let env = read(&[("AGENT_PROVIDER", "ollama")]);
        let error = provider_from_env(&Reader::new(&env), None).unwrap_err();
        assert!(error.to_string().contains("AGENT_MODEL"), "{error}");
        assert!(
            error.to_string().contains("qwen3:8b"),
            "the hint suggests a value: {error}"
        );
    }

    #[test]
    fn anthropic_without_a_key_is_rejected_up_front() {
        let env = read(&[
            ("AGENT_PROVIDER", "anthropic"),
            ("AGENT_MODEL", "claude-sonnet-5"),
        ]);
        let error = provider_from_env(&Reader::new(&env), None).unwrap_err();
        assert!(error.to_string().contains("AGENT_API_KEY"), "{error}");
    }

    #[test]
    fn an_unset_provider_fails_at_startup_and_names_the_variable() {
        let error = provider_from_env(&Reader::new(&read(&[("AGENT_MODEL", "m")])), None)
            .unwrap_err()
            .to_string();
        assert!(
            error.starts_with("AGENT_PROVIDER is not set"),
            "names the variable to set: {error}"
        );
        for name in ["anthropic", "openai", "ollama"] {
            assert!(error.contains(name), "lists `{name}` as a choice: {error}");
        }

        let error = provider_from_env(
            &Reader::new(&read(&[("AGENT_PROVIDER_LOCAL_MODEL", "m")])),
            Some("local"),
        )
        .unwrap_err()
        .to_string();
        assert!(
            error.starts_with("AGENT_PROVIDER_LOCAL_KIND is not set"),
            "an alias needs its kind just the same: {error}"
        );
    }

    #[test]
    fn ollama_never_borrows_the_openai_key() {
        let env = read(&[
            ("AGENT_PROVIDER", "ollama"),
            ("AGENT_MODEL", "qwen3:8b"),
            ("OPENAI_API_KEY", "sk-real-openai-key"),
        ]);
        let provider = provider_from_env(&Reader::new(&env), None).unwrap();
        assert_eq!(
            provider.api_key, None,
            "an OpenAI key must not be sent to the Ollama address"
        );

        let env = read(&[
            ("AGENT_PROVIDER_LOCAL_KIND", "ollama"),
            ("AGENT_PROVIDER_LOCAL_MODEL", "qwen3:8b"),
            ("OPENAI_API_KEY", "sk-real-openai-key"),
        ]);
        let provider = provider_from_env(&Reader::new(&env), Some("local")).unwrap();
        assert_eq!(provider.api_key, None, "nor through an alias");
    }

    #[test]
    fn ollama_sends_a_key_only_when_given_explicitly() {
        let env = read(&[
            ("AGENT_PROVIDER", "ollama"),
            ("AGENT_MODEL", "qwen3:8b"),
            ("AGENT_API_KEY", "proxy-token"),
            ("OPENAI_API_KEY", "sk-real-openai-key"),
        ]);
        let provider = provider_from_env(&Reader::new(&env), None).unwrap();
        assert_eq!(provider.api_key.as_deref(), Some("proxy-token"));

        let env = read(&[
            ("AGENT_PROVIDER_LOCAL_KIND", "ollama"),
            ("AGENT_PROVIDER_LOCAL_MODEL", "qwen3:8b"),
            ("AGENT_PROVIDER_LOCAL_API_KEY", "proxy-token"),
        ]);
        let provider = provider_from_env(&Reader::new(&env), Some("local")).unwrap();
        assert_eq!(provider.api_key.as_deref(), Some("proxy-token"));
    }

    fn provider(pairs: &[(&str, &str)], alias: Option<&str>) -> ProviderSettings {
        provider_from_env(&Reader::new(&read(pairs)), alias)
            .unwrap_or_else(|error| panic!("{pairs:?} should configure a provider: {error}"))
    }

    fn rejection(pairs: &[(&str, &str)], alias: Option<&str>) -> String {
        match provider_from_env(&Reader::new(&read(pairs)), alias) {
            Ok(provider) => panic!("{pairs:?} should be rejected, got {provider:?}"),
            Err(error) => error.to_string(),
        }
    }

    #[test]
    fn anthropic_needs_only_provider_key_and_model() {
        let provider = provider(
            &[
                ("AGENT_PROVIDER", "anthropic"),
                ("AGENT_API_KEY", "sk-ant-x"),
                ("AGENT_MODEL", "claude-sonnet-5-5"),
            ],
            None,
        );
        assert_eq!(provider.kind, ProviderKind::Anthropic);
        assert_eq!(provider.base_url, "https://api.anthropic.com");
        assert_eq!(provider.api_key.as_deref(), Some("sk-ant-x"));
    }

    #[test]
    fn openai_needs_only_provider_key_and_model() {
        let provider = provider(
            &[
                ("AGENT_PROVIDER", "openai"),
                ("AGENT_API_KEY", "sk-x"),
                ("AGENT_MODEL", "some-openai-model"),
            ],
            None,
        );
        assert_eq!(provider.kind, ProviderKind::OpenAiCompatible);
        assert_eq!(
            provider.base_url, "https://api.openai.com/v1",
            "`openai` means the OpenAI API, not a local port"
        );
    }

    #[test]
    fn ollama_needs_only_provider_and_model() {
        let provider = provider(
            &[("AGENT_PROVIDER", "ollama"), ("AGENT_MODEL", "qwen3:8b")],
            None,
        );
        assert_eq!(provider.kind, ProviderKind::OpenAiCompatible);
        assert_eq!(provider.base_url, "http://ollama:11434/v1");
        assert_eq!(provider.api_key, None);
    }

    #[test]
    fn openai_without_a_key_fails_at_startup_and_names_the_key() {
        let error = rejection(&[("AGENT_PROVIDER", "openai"), ("AGENT_MODEL", "m")], None);
        assert!(
            error.starts_with("AGENT_API_KEY is not set"),
            "names the variable to set: {error}"
        );
        assert!(
            error.contains("AGENT_BASE_URL"),
            "says how to reach a keyless server instead: {error}"
        );
    }

    #[test]
    fn a_compatible_server_at_an_explicit_address_needs_no_key() {
        let provider = provider(
            &[
                ("AGENT_PROVIDER", "openai"),
                ("AGENT_BASE_URL", "http://vllm:8000/v1"),
                ("AGENT_MODEL", "m"),
            ],
            None,
        );
        assert_eq!(provider.base_url, "http://vllm:8000/v1");
        assert_eq!(provider.api_key, None);
    }

    #[test]
    fn an_explicit_base_url_always_wins() {
        for name in ["openai", "ollama", "anthropic"] {
            let provider = provider(
                &[
                    ("AGENT_PROVIDER", name),
                    ("AGENT_BASE_URL", "http://proxy:9000/v1"),
                    ("AGENT_API_KEY", "k"),
                    ("AGENT_MODEL", "m"),
                ],
                None,
            );
            assert_eq!(provider.base_url, "http://proxy:9000/v1", "for `{name}`");
        }
    }

    #[test]
    fn aliases_resolve_endpoints_and_keys_by_the_same_rules() {
        let openai = provider(
            &[
                ("AGENT_PROVIDER_GPT_KIND", "openai"),
                ("AGENT_PROVIDER_GPT_API_KEY", "sk-x"),
                ("AGENT_PROVIDER_GPT_MODEL", "m"),
            ],
            Some("gpt"),
        );
        assert_eq!(openai.base_url, "https://api.openai.com/v1");

        let local = provider(
            &[
                ("AGENT_PROVIDER_LOCAL_KIND", "ollama"),
                ("AGENT_PROVIDER_LOCAL_MODEL", "qwen3:8b"),
            ],
            Some("local"),
        );
        assert_eq!(local.base_url, "http://ollama:11434/v1");
        assert_eq!(local.api_key, None);

        let custom = provider(
            &[
                ("AGENT_PROVIDER_GPT_KIND", "openai"),
                ("AGENT_PROVIDER_GPT_BASE_URL", "http://vllm:8000/v1"),
                ("AGENT_PROVIDER_GPT_MODEL", "m"),
            ],
            Some("gpt"),
        );
        assert_eq!(custom.base_url, "http://vllm:8000/v1");

        let error = rejection(
            &[
                ("AGENT_PROVIDER_GPT_KIND", "openai"),
                ("AGENT_PROVIDER_GPT_MODEL", "m"),
            ],
            Some("gpt"),
        );
        assert!(
            error.starts_with("AGENT_PROVIDER_GPT_API_KEY is not set"),
            "names the aliased variable: {error}"
        );
        assert!(error.contains("AGENT_PROVIDER_GPT_BASE_URL"), "{error}");
    }

    #[test]
    fn the_missing_model_hint_fits_the_provider() {
        let anthropic = rejection(
            &[("AGENT_PROVIDER", "anthropic"), ("AGENT_API_KEY", "k")],
            None,
        );
        assert!(anthropic.contains("claude-sonnet-5-5"), "{anthropic}");

        let ollama = rejection(&[("AGENT_PROVIDER", "ollama")], None);
        assert!(ollama.contains("qwen3:8b"), "{ollama}");

        let openai = rejection(
            &[("AGENT_PROVIDER", "openai"), ("AGENT_API_KEY", "k")],
            None,
        );
        assert!(openai.contains("OpenAI model ID"), "{openai}");
        assert!(
            !openai.contains("qwen") && !openai.contains("claude"),
            "no example from another vendor: {openai}"
        );
    }
}
