//! Configuration.
//!
//! Two shapes are supported, and the richer one is a superset of the simpler:
//!
//! * **single provider** - `AGENT_PROVIDER`, `AGENT_BASE_URL`, `AGENT_MODEL`,
//!   `AGENT_API_KEY`. Enough to talk to one Ollama or one cloud endpoint.
//! * **multiple providers** - `AGENT_PROVIDERS=local,cloud` plus
//!   `AGENT_PROVIDER_<ALIAS>_*` for each. This is what turns the routing seam
//!   on; the single-provider form is the degenerate case of one entry behind a
//!   static router.
//!
//! Parsing is driven by [`EnvSource`], so all of it is testable without
//! touching the process environment - see [`env`].

pub mod env;
pub mod kinds;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use agent_domain::model::llm::{ModelId, ProviderId};
use thiserror::Error;

pub use env::{EnvSource, MapEnv, SystemEnv};
pub use kinds::{ApprovalPolicy, ProviderKind, RouterKind};

use env::{Reader, provider_from_env};

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("{key} is not set. {hint}")]
    Missing { key: String, hint: String },

    #[error("{key}=`{value}` is not valid: {reason}")]
    Invalid {
        key: String,
        value: String,
        reason: String,
    },
}

impl ConfigError {
    pub(crate) fn missing(key: impl Into<String>, hint: impl Into<String>) -> Self {
        Self::Missing {
            key: key.into(),
            hint: hint.into(),
        }
    }

    pub(crate) fn invalid(
        key: impl Into<String>,
        value: impl Into<String>,
        reason: impl Into<String>,
    ) -> Self {
        Self::Invalid {
            key: key.into(),
            value: value.into(),
            reason: reason.into(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ProviderSettings {
    pub id: ProviderId,
    pub kind: ProviderKind,
    pub base_url: String,
    pub api_key: Option<String>,
    pub model: ModelId,
    /// Newer OpenAI models renamed `max_tokens` to `max_completion_tokens`;
    /// self-hosted servers still expect the old name.
    pub max_tokens_field: String,
}

impl ProviderSettings {
    /// API key with everything but the first and last few characters masked, so
    /// `agent doctor` can be pasted into a bug report.
    pub fn masked_api_key(&self) -> String {
        match &self.api_key {
            None => "(none)".to_string(),
            Some(key) if key.len() <= 8 => "*".repeat(key.len()),
            Some(key) => format!("{}…{}", &key[..4], &key[key.len() - 4..]),
        }
    }
}

#[derive(Debug, Clone)]
pub struct LlmSettings {
    pub providers: Vec<ProviderSettings>,
    pub default_provider: ProviderId,
    pub router: RouterKind,
    pub request_timeout: Duration,
    pub max_retries: u32,
}

#[derive(Debug, Clone)]
pub struct LoopSettings {
    pub max_iterations: u32,
    pub max_tool_output_bytes: usize,
    pub max_history_bytes: usize,
    pub tool_timeout: Duration,
    pub parallel_read_only_tools: bool,
    pub stream: bool,
    pub temperature: Option<f32>,
    pub max_tokens: Option<u32>,
}

#[derive(Debug, Clone)]
pub struct Settings {
    pub workspace: PathBuf,
    pub approval: ApprovalPolicy,
    pub llm: LlmSettings,
    pub agent_loop: LoopSettings,
    /// Largest file the read tool will load.
    pub max_file_bytes: u64,
}

impl Settings {
    /// Reads the process environment.
    pub fn from_env() -> Result<Self, ConfigError> {
        Self::from_source(&SystemEnv)
    }

    pub fn from_source(source: &dyn EnvSource) -> Result<Self, ConfigError> {
        let reader = Reader::new(source);

        let workspace = match reader.string("AGENT_WORKSPACE") {
            Some(path) => PathBuf::from(path),
            None => std::env::current_dir().map_err(|error| {
                ConfigError::invalid("AGENT_WORKSPACE", "", format!("no current dir: {error}"))
            })?,
        };

        Ok(Self {
            workspace,
            approval: reader.parsed("AGENT_APPROVAL", ApprovalPolicy::ReadOnlyAuto)?,
            llm: LlmSettings::read(&reader)?,
            agent_loop: LoopSettings::read(&reader)?,
            max_file_bytes: reader.parsed("AGENT_MAX_FILE_BYTES", 2 * 1024 * 1024)?,
        })
    }

    /// The provider a request reaches when nothing steers it elsewhere.
    pub fn default_provider_settings(&self) -> &ProviderSettings {
        self.llm
            .providers
            .iter()
            .find(|provider| provider.id == self.llm.default_provider)
            .unwrap_or_else(|| &self.llm.providers[0])
    }
}

impl LoopSettings {
    fn read(reader: &Reader<'_>) -> Result<Self, ConfigError> {
        Ok(Self {
            max_iterations: reader.parsed("AGENT_MAX_ITERATIONS", 25)?,
            max_tool_output_bytes: reader.parsed("AGENT_MAX_TOOL_OUTPUT_BYTES", 32 * 1024)?,
            max_history_bytes: reader.parsed("AGENT_MAX_HISTORY_BYTES", 256 * 1024)?,
            tool_timeout: Duration::from_secs(reader.parsed("AGENT_TOOL_TIMEOUT_SECS", 60)?),
            parallel_read_only_tools: reader.parsed("AGENT_PARALLEL_READ_TOOLS", true)?,
            stream: reader.parsed("AGENT_STREAM", true)?,
            temperature: Some(reader.optional("AGENT_TEMPERATURE")?.unwrap_or(0.2)),
            max_tokens: Some(reader.optional("AGENT_MAX_TOKENS")?.unwrap_or(4096)),
        })
    }
}

impl LlmSettings {
    fn read(reader: &Reader<'_>) -> Result<Self, ConfigError> {
        let aliases: Vec<String> = reader
            .string("AGENT_PROVIDERS")
            .map(|raw| {
                raw.split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(String::from)
                    .collect()
            })
            .unwrap_or_default();

        let providers = if aliases.is_empty() {
            vec![provider_from_env(reader, None)?]
        } else {
            aliases
                .iter()
                .map(|alias| provider_from_env(reader, Some(alias)))
                .collect::<Result<_, _>>()?
        };

        let default_provider = match reader.string("AGENT_DEFAULT_PROVIDER") {
            Some(raw) => {
                let id = ProviderId::new(raw.clone());
                if !providers.iter().any(|provider| provider.id == id) {
                    return Err(ConfigError::invalid(
                        "AGENT_DEFAULT_PROVIDER",
                        raw,
                        format!("not among the configured providers: {}", names(&providers)),
                    ));
                }
                id
            }
            None => providers[0].id.clone(),
        };

        // One provider has nothing to route between, so the default is static;
        // more than one defaults to letting the model reference pick.
        let default_router = if providers.len() > 1 {
            RouterKind::ModelPrefix
        } else {
            RouterKind::Static
        };

        Ok(Self {
            providers,
            default_provider,
            router: reader.parsed("AGENT_ROUTER", default_router)?,
            request_timeout: Duration::from_secs(reader.parsed("AGENT_REQUEST_TIMEOUT_SECS", 180)?),
            max_retries: reader.parsed("AGENT_MAX_RETRIES", 3)?,
        })
    }
}

fn names(providers: &[ProviderSettings]) -> String {
    providers
        .iter()
        .map(|provider| provider.id.to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Renders the resolved configuration for `agent doctor`, with secrets masked.
pub fn describe(settings: &Settings) -> BTreeMap<String, String> {
    let mut description = BTreeMap::new();
    description.insert("workspace".into(), settings.workspace.display().to_string());
    description.insert("approval".into(), settings.approval.as_str().into());
    description.insert("router".into(), settings.llm.router.as_str().into());
    description.insert(
        "default provider".into(),
        settings.llm.default_provider.to_string(),
    );
    description.insert(
        "max iterations".into(),
        settings.agent_loop.max_iterations.to_string(),
    );
    for provider in &settings.llm.providers {
        description.insert(
            format!("provider[{}]", provider.id),
            format!(
                "{} model={} base_url={} api_key={}",
                provider.kind.as_str(),
                provider.model,
                provider.base_url,
                provider.masked_api_key()
            ),
        );
    }
    description
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(pairs: &[(&str, &str)]) -> Result<Settings, ConfigError> {
        let mut env = MapEnv::new(pairs);
        // Always set, so the tests never depend on the process' cwd.
        if env.get("AGENT_WORKSPACE").is_none() {
            env.set("AGENT_WORKSPACE", "/workspace");
        }
        Settings::from_source(&env)
    }

    #[test]
    fn a_single_provider_defaults_to_a_static_router() {
        let settings = settings(&[("AGENT_MODEL", "qwen3:8b")]).unwrap();

        assert_eq!(settings.llm.providers.len(), 1);
        assert_eq!(settings.llm.router, RouterKind::Static);
        assert_eq!(settings.llm.default_provider.as_str(), "default");
        assert_eq!(settings.approval, ApprovalPolicy::ReadOnlyAuto);
        assert_eq!(settings.agent_loop.max_iterations, 25);
    }

    #[test]
    fn several_providers_default_to_the_model_prefix_router() {
        let settings = settings(&[
            ("AGENT_PROVIDERS", "local, cloud"),
            ("AGENT_PROVIDER_LOCAL_MODEL", "qwen3:8b"),
            ("AGENT_PROVIDER_CLOUD_KIND", "anthropic"),
            ("AGENT_PROVIDER_CLOUD_MODEL", "claude-sonnet-5"),
            ("AGENT_PROVIDER_CLOUD_API_KEY", "sk-ant-x"),
        ])
        .unwrap();

        assert_eq!(settings.llm.providers.len(), 2);
        assert_eq!(settings.llm.router, RouterKind::ModelPrefix);
        assert_eq!(
            settings.llm.default_provider.as_str(),
            "local",
            "the first alias wins"
        );
        assert_eq!(
            settings.default_provider_settings().model.as_str(),
            "qwen3:8b"
        );
    }

    #[test]
    fn an_explicit_default_provider_must_exist() {
        let error = settings(&[
            ("AGENT_PROVIDERS", "local"),
            ("AGENT_PROVIDER_LOCAL_MODEL", "qwen3:8b"),
            ("AGENT_DEFAULT_PROVIDER", "cloud"),
        ])
        .unwrap_err();

        assert!(
            error.to_string().contains("AGENT_DEFAULT_PROVIDER"),
            "{error}"
        );
        assert!(
            error.to_string().contains("local"),
            "the message lists what exists: {error}"
        );
    }

    #[test]
    fn an_explicit_router_overrides_the_default() {
        let settings = settings(&[("AGENT_MODEL", "m"), ("AGENT_ROUTER", "model-prefix")]).unwrap();
        assert_eq!(settings.llm.router, RouterKind::ModelPrefix);
    }

    #[test]
    fn loop_limits_come_from_the_environment() {
        let settings = settings(&[
            ("AGENT_MODEL", "m"),
            ("AGENT_MAX_ITERATIONS", "3"),
            ("AGENT_TOOL_TIMEOUT_SECS", "7"),
            ("AGENT_PARALLEL_READ_TOOLS", "false"),
            ("AGENT_STREAM", "false"),
            ("AGENT_TEMPERATURE", "0.9"),
        ])
        .unwrap();

        assert_eq!(settings.agent_loop.max_iterations, 3);
        assert_eq!(settings.agent_loop.tool_timeout, Duration::from_secs(7));
        assert!(!settings.agent_loop.parallel_read_only_tools);
        assert!(!settings.agent_loop.stream);
        assert_eq!(settings.agent_loop.temperature, Some(0.9));
    }

    #[test]
    fn describe_masks_secrets() {
        let settings = settings(&[
            ("AGENT_PROVIDER", "anthropic"),
            ("AGENT_MODEL", "claude-sonnet-5"),
            ("AGENT_API_KEY", "sk-ant-0123456789abcdef"),
        ])
        .unwrap();

        let rendered = describe(&settings)
            .values()
            .cloned()
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            !rendered.contains("0123456789"),
            "the key must not be printed: {rendered}"
        );
        assert!(rendered.contains("sk-a"), "{rendered}");
    }

    #[test]
    fn api_keys_are_masked() {
        let provider = ProviderSettings {
            id: ProviderId::new("cloud"),
            kind: ProviderKind::Anthropic,
            base_url: "https://api.anthropic.com".into(),
            api_key: Some("sk-ant-0123456789abcdef".into()),
            model: ModelId::new("claude-sonnet-5"),
            max_tokens_field: "max_tokens".into(),
        };
        let masked = provider.masked_api_key();
        assert!(masked.starts_with("sk-a"));
        assert!(masked.ends_with("cdef"));
        assert!(!masked.contains("0123456789"));
    }

    #[test]
    fn short_keys_are_fully_masked() {
        let mut provider = ProviderSettings {
            id: ProviderId::new("x"),
            kind: ProviderKind::OpenAiCompatible,
            base_url: String::new(),
            api_key: Some("short".into()),
            model: ModelId::new("m"),
            max_tokens_field: "max_tokens".into(),
        };
        assert_eq!(provider.masked_api_key(), "*****");
        provider.api_key = None;
        assert_eq!(provider.masked_api_key(), "(none)");
    }
}
