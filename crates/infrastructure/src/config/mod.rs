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

use crate::exec::SandboxRequirement;

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

/// Operator control over the system prompt.
///
/// The file is read once at startup by the *operator's* process, not through
/// the model's tools, so it may legitimately live outside the workspace
/// sandbox. Replacing the prompt does not weaken the sandbox either - path
/// confinement is enforced in code, never by prompt text.
#[derive(Debug, Clone, Default)]
pub struct PromptSettings {
    /// Replace the built-in system prompt with this file's contents.
    pub replace_file: Option<PathBuf>,
    /// Append these instructions to the end of the prompt (after the project
    /// instruction file; combined with `replace_file`, appends to that text).
    pub append: Option<String>,
}

/// Outbound web access for the `web_fetch` tool.
///
/// Off by default: giving the model a network egress is an explicit operator
/// decision (a URL can carry anything the model puts in it). Even when
/// enabled, private and internal addresses stay blocked unless
/// `allow_private` is also set, and the tool's `Network` safety class means
/// the default approval policy confirms every call.
#[derive(Debug, Clone)]
pub struct WebFetchSettings {
    pub enabled: bool,
    /// Domain suffixes the operator allows. Empty = any public host.
    pub allowed_domains: Vec<String>,
    /// Lifts the private/internal-address blocking (intranet docs, tests).
    pub allow_private: bool,
    pub max_bytes: usize,
    pub timeout: Duration,
}

impl Default for WebFetchSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            allowed_domains: Vec::new(),
            allow_private: false,
            max_bytes: 1024 * 1024,
            timeout: Duration::from_secs(30),
        }
    }
}

/// The `run_command` tool.
///
/// Off by default, and for a stronger reason than `web_fetch`: a command is
/// the one capability whose effect cannot be read off its arguments. Enabling
/// it is a decision about how much of the machine the model may touch.
///
/// `sandbox` is a requirement, not a preference. If the platform cannot meet
/// it, startup fails rather than quietly running commands with less
/// confinement than the operator asked for - the failure mode that would make
/// the setting worthless.
#[derive(Debug, Clone, Default)]
pub struct ShellSettings {
    /// Off unless the operator says otherwise.
    pub enabled: bool,
    /// The confinement the operator will not run without.
    pub sandbox: SandboxRequirement,
    /// Domain suffixes commands may reach through the egress proxy. Empty
    /// means no network at all, which is the default.
    pub allowed_domains: Vec<String>,
    /// Writable roots outside the workspace - build caches such as a
    /// `CARGO_TARGET_DIR` that points elsewhere.
    pub extra_writable: Vec<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct Settings {
    pub workspace: PathBuf,
    pub approval: ApprovalPolicy,
    pub llm: LlmSettings,
    pub agent_loop: LoopSettings,
    pub prompt: PromptSettings,
    pub web_fetch: WebFetchSettings,
    pub shell: ShellSettings,
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
            prompt: PromptSettings {
                replace_file: reader.string("AGENT_SYSTEM_PROMPT_FILE").map(PathBuf::from),
                append: reader.string("AGENT_APPEND_SYSTEM_PROMPT"),
            },
            web_fetch: WebFetchSettings {
                enabled: reader.parsed("AGENT_WEB_FETCH", false)?,
                allowed_domains: reader.list("AGENT_WEB_FETCH_ALLOW"),
                allow_private: reader.parsed("AGENT_WEB_FETCH_ALLOW_PRIVATE", false)?,
                max_bytes: reader.parsed("AGENT_WEB_FETCH_MAX_BYTES", 1024 * 1024)?,
                timeout: Duration::from_secs(reader.parsed("AGENT_WEB_FETCH_TIMEOUT_SECS", 30)?),
            },
            shell: ShellSettings {
                enabled: reader.parsed("AGENT_SHELL", false)?,
                sandbox: reader.parsed("AGENT_SHELL_SANDBOX", SandboxRequirement::default())?,
                allowed_domains: reader.list("AGENT_SHELL_NETWORK_ALLOW"),
                extra_writable: reader
                    .list("AGENT_SHELL_WRITABLE")
                    .into_iter()
                    .map(PathBuf::from)
                    .collect(),
            },
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
    description.insert(
        "web fetch".into(),
        if !settings.web_fetch.enabled {
            "disabled".to_string()
        } else {
            let scope = if settings.web_fetch.allowed_domains.is_empty() {
                "any public host".to_string()
            } else {
                format!(
                    "allowlist: {}",
                    settings.web_fetch.allowed_domains.join(", ")
                )
            };
            if settings.web_fetch.allow_private {
                format!("enabled ({scope}; private addresses ALLOWED)")
            } else {
                format!("enabled ({scope})")
            }
        },
    );
    description.insert(
        "run_command".into(),
        if !settings.shell.enabled {
            "disabled".to_string()
        } else {
            let egress = if settings.shell.allowed_domains.is_empty() {
                "no network".to_string()
            } else {
                format!("allowlist: {}", settings.shell.allowed_domains.join(", "))
            };
            format!("enabled (sandbox: {}; {egress})", settings.shell.sandbox)
        },
    );
    description.insert(
        "system prompt".into(),
        match (&settings.prompt.replace_file, &settings.prompt.append) {
            (None, None) => "built-in".to_string(),
            (Some(path), None) => format!("replaced from {}", path.display()),
            (None, Some(_)) => "built-in + appended instructions".to_string(),
            (Some(path), Some(_)) => {
                format!("replaced from {} + appended instructions", path.display())
            }
        },
    );
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
    fn the_shell_tool_is_off_and_networkless_by_default() {
        // Both halves matter: forgetting either one is the difference between
        // "no shell" and "a shell that can reach the internet".
        let settings = settings(&[("AGENT_MODEL", "m")]).unwrap();

        assert!(!settings.shell.enabled);
        assert!(settings.shell.allowed_domains.is_empty());
        assert_eq!(settings.shell.sandbox, SandboxRequirement::Confined);
    }

    #[test]
    fn the_shell_allowlist_and_writable_roots_are_comma_separated() {
        let settings = settings(&[
            ("AGENT_MODEL", "m"),
            ("AGENT_SHELL", "true"),
            (
                "AGENT_SHELL_NETWORK_ALLOW",
                "crates.io, static.crates.io , ",
            ),
            ("AGENT_SHELL_WRITABLE", "/cache,/workspace/target"),
        ])
        .unwrap();

        assert!(settings.shell.enabled);
        assert_eq!(
            settings.shell.allowed_domains,
            vec!["crates.io".to_string(), "static.crates.io".to_string()],
            "blank entries must be dropped rather than becoming an empty allowlist entry"
        );
        assert_eq!(settings.shell.extra_writable.len(), 2);
    }

    #[test]
    fn turning_the_sandbox_off_has_to_be_spelled_out() {
        assert_eq!(
            settings(&[("AGENT_MODEL", "m"), ("AGENT_SHELL_SANDBOX", "none")])
                .unwrap()
                .shell
                .sandbox,
            SandboxRequirement::Disabled
        );

        // A typo must not read as "off". Anything unrecognised is an error.
        let error = settings(&[("AGENT_MODEL", "m"), ("AGENT_SHELL_SANDBOX", "nome")])
            .expect_err("an unknown sandbox setting must abort startup");
        assert!(error.to_string().contains("AGENT_SHELL_SANDBOX"), "{error}");
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
    fn prompt_injection_is_read_from_the_environment() {
        let settings = settings(&[
            ("AGENT_MODEL", "m"),
            ("AGENT_SYSTEM_PROMPT_FILE", "/prompts/agent.md"),
            ("AGENT_APPEND_SYSTEM_PROMPT", "Reply in Japanese."),
        ])
        .unwrap();

        assert_eq!(
            settings.prompt.replace_file,
            Some(PathBuf::from("/prompts/agent.md"))
        );
        assert_eq!(
            settings.prompt.append.as_deref(),
            Some("Reply in Japanese.")
        );

        let rendered = describe(&settings);
        assert!(
            rendered["system prompt"].contains("/prompts/agent.md"),
            "doctor shows where the prompt comes from: {rendered:?}"
        );
    }

    #[test]
    fn the_prompt_is_built_in_when_nothing_is_injected() {
        let settings = settings(&[("AGENT_MODEL", "m")]).unwrap();
        assert_eq!(settings.prompt.replace_file, None);
        assert_eq!(settings.prompt.append, None);
        assert_eq!(describe(&settings)["system prompt"], "built-in");
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
