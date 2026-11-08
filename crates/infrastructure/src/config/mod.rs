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

mod access;
mod describe;
pub mod env;
pub mod kinds;
mod permissions;
pub mod routes;

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::session::store::RetentionPolicy;

use agent_domain::model::llm::{ModelId, ProviderId};
use agent_domain::ports::notify::NotificationKind;
use thiserror::Error;

use crate::exec::SandboxRequirement;
use crate::llm::{Effort, PromptCache, Thinking};
use crate::notify::{NotifyFormat, NtfyToken, WebhookUrl};
use crate::web::SearchApiKey;

pub use access::AccessSettings;
pub use describe::describe;
pub use env::{EnvSource, MapEnv, SystemEnv};
pub use kinds::{ApprovalPolicy, ProviderKind, RouterKind};
pub use permissions::{
    MAX_PERMISSIONS_BYTES, MAX_RULES, PermissionsError, load_permissions, parse_permissions,
};
pub use routes::RoutingSettings;

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
    /// Prompt-cache breakpoints for Anthropic providers. OpenAI-compatible
    /// servers that cache do so on their own, so there is nothing to send them.
    pub prompt_cache: PromptCache,
    /// Extended thinking for Anthropic providers. Off leaves the field out of
    /// the request entirely.
    pub thinking: Thinking,
    /// `output_config.effort` for Anthropic providers; `None` sends nothing.
    pub effort: Option<Effort>,
    /// Per-task destinations and the failover chain.
    pub routing: RoutingSettings,
}

#[derive(Debug, Clone)]
pub struct LoopSettings {
    pub max_iterations: u32,
    pub max_tool_output_bytes: usize,
    pub max_history_bytes: usize,
    /// Fold the oldest turns into a summary before resorting to dropping them.
    pub compact: bool,
    /// Most messages left verbatim after a compaction.
    pub compact_keep_recent: usize,
    /// Most bytes left verbatim after a compaction. The cap that usually
    /// binds, since a message count says nothing about how big the messages
    /// are.
    pub compact_keep_recent_bytes: usize,
    pub tool_timeout: Duration,
    pub parallel_read_only_tools: bool,
    pub stream: bool,
    pub temperature: Option<f32>,
    pub max_tokens: Option<u32>,
}

/// `spawn_agent`: read-only sub-agents the model can hand an investigation
/// to.
///
/// On by default: a sub-agent can do nothing the model could not already do
/// with its own read-only tools, and every call it makes goes through the
/// same approval gate. What it adds is cost - a sub-agent's requests are
/// tokens too - so it has its own bounds rather than the parent's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubagentSettings {
    pub enabled: bool,
    /// The sub-agent's own iteration budget, independent of the parent's.
    pub max_iterations: u32,
    /// The sub-agent's own history budget. Smaller than the parent's by
    /// default: it starts empty and is meant to finish quickly.
    pub max_history_bytes: usize,
    /// Longest answer handed back to the parent.
    pub max_answer_bytes: usize,
}

impl SubagentSettings {
    fn read(reader: &Reader<'_>) -> Result<Self, ConfigError> {
        Ok(Self {
            enabled: reader.parsed("AGENT_SUBAGENTS", true)?,
            // At least one round trip, as for the parent's budget.
            max_iterations: reader
                .parsed("AGENT_SUBAGENT_MAX_ITERATIONS", 15u32)?
                .max(1),
            max_history_bytes: reader.parsed("AGENT_SUBAGENT_MAX_HISTORY_BYTES", 128 * 1024)?,
            max_answer_bytes: reader.parsed("AGENT_SUBAGENT_MAX_ANSWER_BYTES", 8 * 1024)?,
        })
    }
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
    /// The operator's own instruction file, merged in front of the
    /// workspace's (`AGENT_GLOBAL_INSTRUCTIONS`). Always absolute: there is
    /// no directory a relative path could sensibly be read against, since
    /// the same file is meant for every workspace.
    pub global_instructions: Option<PathBuf>,
}

const GLOBAL_INSTRUCTIONS_KEY: &str = "AGENT_GLOBAL_INSTRUCTIONS";

impl PromptSettings {
    fn read(reader: &Reader<'_>) -> Result<Self, ConfigError> {
        let global_instructions = match reader.string(GLOBAL_INSTRUCTIONS_KEY) {
            None => None,
            Some(raw) if Path::new(&raw).is_absolute() => Some(PathBuf::from(raw)),
            Some(raw) => {
                return Err(ConfigError::invalid(
                    GLOBAL_INSTRUCTIONS_KEY,
                    raw,
                    "must be an absolute path (`~` is not expanded), e.g. \
                     /home/me/.config/agent/AGENTS.md",
                ));
            }
        };
        Ok(Self {
            replace_file: reader.string("AGENT_SYSTEM_PROMPT_FILE").map(PathBuf::from),
            append: reader.string("AGENT_APPEND_SYSTEM_PROMPT"),
            global_instructions,
        })
    }
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

/// The `web_search` tool (Brave Search API).
///
/// Off by default for the same reason as `web_fetch`: every query is a
/// message the model sends to a third party. `Debug` is safe to print -
/// [`SearchApiKey`] hides the key.
#[derive(Debug, Clone, Default)]
pub struct WebSearchSettings {
    pub enabled: bool,
    pub api_key: Option<SearchApiKey>,
}

const WEB_SEARCH_KEY: &str = "AGENT_WEB_SEARCH";
const WEB_SEARCH_API_KEY_KEY: &str = "AGENT_WEB_SEARCH_API_KEY";

impl WebSearchSettings {
    fn read(reader: &Reader<'_>) -> Result<Self, ConfigError> {
        let enabled = reader.parsed(WEB_SEARCH_KEY, false)?;
        let api_key = reader.string(WEB_SEARCH_API_KEY_KEY).map(SearchApiKey::new);
        if enabled && api_key.is_none() {
            return Err(ConfigError::missing(
                WEB_SEARCH_API_KEY_KEY,
                "AGENT_WEB_SEARCH=true needs a Brave Search API key \
                 (https://brave.com/search/api/). Set it, or unset AGENT_WEB_SEARCH",
            ));
        }
        Ok(Self { enabled, api_key })
    }

    /// The key to search with, when searching is on.
    pub fn active_key(&self) -> Option<&SearchApiKey> {
        self.api_key.as_ref().filter(|_| self.enabled)
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
    /// `start_command` / `read_output` / `kill_command`.
    pub background: BackgroundSettings,
}

/// Background jobs. Offered only alongside `run_command`, and only where its
/// sandbox actually confines - a job runs unattended between calls.
///
/// Every limit is finite: there is no setting for "no lifetime" or "keep all
/// output", because a job without them is a process the agent no longer
/// controls.
#[derive(Debug, Clone)]
pub struct BackgroundSettings {
    /// On by default once the shell is: the shell switch is the decision
    /// about running commands at all.
    pub enabled: bool,
    pub max_jobs: usize,
    pub max_lifetime: Duration,
    pub buffer_bytes: usize,
}

impl Default for BackgroundSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            max_jobs: crate::exec::background::DEFAULT_MAX_JOBS,
            max_lifetime: crate::exec::background::DEFAULT_MAX_LIFETIME,
            buffer_bytes: crate::exec::background::DEFAULT_BUFFER_BYTES,
        }
    }
}

const BACKGROUND_KEY: &str = "AGENT_SHELL_BACKGROUND";
const BACKGROUND_MAX_JOBS_KEY: &str = "AGENT_SHELL_BACKGROUND_MAX_JOBS";
const BACKGROUND_LIFETIME_KEY: &str = "AGENT_SHELL_BACKGROUND_LIFETIME_SECS";
const BACKGROUND_BUFFER_KEY: &str = "AGENT_SHELL_BACKGROUND_BUFFER_BYTES";

impl BackgroundSettings {
    fn read(reader: &Reader<'_>) -> Result<Self, ConfigError> {
        let defaults = Self::default();
        // Zero would mean "no jobs", "dead on arrival" or "no output" - none
        // of them what an operator typing a number meant. The off switch is
        // its own variable.
        let positive = |key: &str, fallback: usize| -> Result<usize, ConfigError> {
            match reader.parsed(key, fallback)? {
                0 => Err(ConfigError::invalid(
                    key,
                    "0",
                    "must be at least 1; set AGENT_SHELL_BACKGROUND=false to turn background \
                     jobs off",
                )),
                value => Ok(value),
            }
        };
        Ok(Self {
            enabled: reader.parsed(BACKGROUND_KEY, defaults.enabled)?,
            max_jobs: positive(BACKGROUND_MAX_JOBS_KEY, defaults.max_jobs)?,
            max_lifetime: Duration::from_secs(positive(
                BACKGROUND_LIFETIME_KEY,
                defaults.max_lifetime.as_secs() as usize,
            )? as u64),
            buffer_bytes: positive(BACKGROUND_BUFFER_KEY, defaults.buffer_bytes)?,
        })
    }
}

/// MCP servers (stdio), each started inside the OS sandbox.
///
/// Off unless `config` is set. The file is the operator's, read once at
/// startup, and must live outside the workspace: a configuration a cloned
/// repository could ship would start code of the repository's choosing the
/// moment the agent is launched in it.
///
/// `sandbox` is separate from the shell's on purpose. Turning the sandbox off
/// for `run_command` in a disposable container is a decision about commands
/// the human approves one by one; a server runs unattended between calls,
/// so there is no `none` here.
#[derive(Debug, Clone, Default)]
pub struct McpSettings {
    /// Absolute path of the JSON file listing the servers.
    pub config: Option<PathBuf>,
    /// The confinement every server runs under. Never `Disabled`.
    pub sandbox: SandboxRequirement,
}

const MCP_CONFIG_KEY: &str = "AGENT_MCP_CONFIG";
const MCP_SANDBOX_KEY: &str = "AGENT_MCP_SANDBOX";

impl McpSettings {
    fn read(reader: &Reader<'_>) -> Result<Self, ConfigError> {
        let config = match reader.string(MCP_CONFIG_KEY) {
            None => None,
            Some(raw) if Path::new(&raw).is_absolute() => Some(PathBuf::from(raw)),
            Some(raw) => {
                return Err(ConfigError::invalid(
                    MCP_CONFIG_KEY,
                    raw,
                    "must be an absolute path outside the workspace (`~` is not expanded), e.g. \
                     /home/me/.config/agent/mcp.json",
                ));
            }
        };
        let sandbox = reader.parsed(MCP_SANDBOX_KEY, SandboxRequirement::default())?;
        if sandbox == SandboxRequirement::Disabled {
            return Err(ConfigError::invalid(
                MCP_SANDBOX_KEY,
                reader.string(MCP_SANDBOX_KEY).unwrap_or_default(),
                "MCP servers always run sandboxed. Use `confined` (the default) or `isolated`, \
                 or unset AGENT_MCP_CONFIG to start no servers",
            ));
        }
        Ok(Self { config, sandbox })
    }
}

/// The operator's hooks: commands run before a tool call is approved, after
/// it ran, and when a turn ends.
///
/// Off unless `config` is set. Like the MCP configuration, the file is the
/// operator's: an absolute path from the real environment (never `.env`),
/// refused when it lives inside the workspace - a hook can refuse calls, so a
/// file the repository or the model could write would decide what the agent
/// may do.
///
/// `sandbox` defaults to confinement, and a platform that cannot provide it
/// fails startup. `none` is allowed, as for `run_command`: the operator wrote
/// the commands, and turning confinement off for them is a deliberate act.
#[derive(Debug, Clone, Default)]
pub struct HookSettings {
    /// Absolute path of the JSON file listing the hooks.
    pub config: Option<PathBuf>,
    pub sandbox: SandboxRequirement,
}

const HOOKS_KEY: &str = "AGENT_HOOKS";
const HOOKS_SANDBOX_KEY: &str = "AGENT_HOOKS_SANDBOX";

impl HookSettings {
    fn read(reader: &Reader<'_>) -> Result<Self, ConfigError> {
        let config = match reader.string(HOOKS_KEY) {
            None => None,
            Some(raw) if Path::new(&raw).is_absolute() => Some(PathBuf::from(raw)),
            Some(raw) => {
                return Err(ConfigError::invalid(
                    HOOKS_KEY,
                    raw,
                    "must be an absolute path outside the workspace (`~` is not expanded), e.g. \
                     /home/me/.config/agent/hooks.json",
                ));
            }
        };
        let sandbox = reader.parsed(HOOKS_SANDBOX_KEY, SandboxRequirement::default())?;
        Ok(Self { config, sandbox })
    }
}

/// What a session leaves on disk.
///
/// Off is a real option and not a formality: the log holds the conversation
/// verbatim, including the contents of every file the model read, so enabling
/// it is a decision about where that material is allowed to sit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionSettings {
    pub log: bool,
    /// Relative paths resolve against the workspace root.
    pub dir: PathBuf,
    /// How long the records are kept. Nothing removes them otherwise: the log
    /// only ever appends, so without this the directory is a ratchet.
    pub retention: RetentionPolicy,
}

/// Push notifications over a webhook.
///
/// Off unless a URL is set, and the URL being set is the operator's consent
/// to what follows: a completed turn's notification carries the opening of
/// the model's answer to a server outside this machine. `excerpt_chars = 0`
/// keeps answers out entirely.
///
/// `Debug` is safe to print: [`WebhookUrl`] hides everything past the host.
#[derive(Debug, Clone)]
pub struct NotifySettings {
    /// `None` wires a notifier that sends nothing.
    pub webhook: Option<WebhookUrl>,
    pub format: NotifyFormat,
    /// What the operator subscribed to. Never empty.
    pub events: Vec<NotificationKind>,
    /// Completed turns shorter than this are not worth a buzz - in `chat`
    /// the person who typed the prompt is usually still there. Approvals and
    /// failures ignore it.
    pub min_turn: Duration,
    /// Characters of the answer to include. `0` sends none.
    pub excerpt_chars: usize,
    /// `AGENT_NOTIFY_NTFY_TOKEN`: sent with every request to an ntfy server.
    /// Only meaningful with the ntfy format.
    pub ntfy_token: Option<NtfyToken>,
    /// Answering prompts from the phone. `None` unless a reply topic is set.
    pub remote_approval: Option<RemoteApprovalSettings>,
}

/// Approving or denying a prompt from the ntfy notification.
///
/// Whoever can read the notification topic can approve: the buttons carry
/// what a reply needs. So this is accepted only together with an access
/// token, and the server is checked at startup to refuse anonymous readers
/// of the notification topic and anonymous writers to the reply topic.
#[derive(Clone)]
pub struct RemoteApprovalSettings {
    /// The topic replies are posted to and read from. A credential, like the
    /// notification topic.
    pub reply_topic: String,
    /// Put in the buttons instead of the main token: it then only needs to
    /// write to the reply topic, which is all a reader of the notification
    /// topic can do with it.
    pub reply_token: Option<NtfyToken>,
    /// How long a prompt's one-time token stays valid. The terminal keeps
    /// waiting after it lapses.
    pub ttl: Duration,
}

impl std::fmt::Debug for RemoteApprovalSettings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RemoteApprovalSettings")
            .field("reply_topic", &"(hidden)")
            .field("reply_token", &self.reply_token)
            .field("ttl", &self.ttl)
            .finish()
    }
}

const NOTIFY_URL_KEY: &str = "AGENT_NOTIFY_WEBHOOK_URL";
const NOTIFY_EVENTS_KEY: &str = "AGENT_NOTIFY_EVENTS";
const NTFY_TOKEN_KEY: &str = "AGENT_NOTIFY_NTFY_TOKEN";
const NTFY_REPLY_TOPIC_KEY: &str = "AGENT_NOTIFY_NTFY_REPLY_TOPIC";
const NTFY_REPLY_TOKEN_KEY: &str = "AGENT_NOTIFY_NTFY_REPLY_TOKEN";
const NTFY_TTL_KEY: &str = "AGENT_NOTIFY_APPROVAL_TTL_SECS";

/// Reads a token without ever repeating it in an error.
fn ntfy_token(reader: &Reader<'_>, key: &str) -> Result<Option<NtfyToken>, ConfigError> {
    reader
        .string(key)
        .map(|raw| {
            NtfyToken::parse(&raw).map_err(|reason| ConfigError::invalid(key, "(hidden)", reason))
        })
        .transpose()
}

impl NotifySettings {
    fn read(reader: &Reader<'_>) -> Result<Self, ConfigError> {
        // The value is the secret, so no error below repeats it.
        let webhook = match reader.string(NOTIFY_URL_KEY) {
            None => None,
            Some(raw) => Some(WebhookUrl::parse(&raw).map_err(|reason| {
                ConfigError::invalid(
                    NOTIFY_URL_KEY,
                    "(hidden)",
                    format!(
                        "{reason}; expected e.g. https://ntfy.sh/<topic> or a Slack \
                         incoming-webhook URL"
                    ),
                )
            })?),
        };

        // An explicit format always wins over the guess from the host.
        let format = match reader.optional::<NotifyFormat>("AGENT_NOTIFY_FORMAT")? {
            Some(format) => format,
            None => webhook
                .as_ref()
                .map_or(NotifyFormat::Json, NotifyFormat::infer),
        };
        let notify_topic = match &webhook {
            Some(url) => {
                crate::notify::endpoint(url, format)
                    .map_err(|reason| ConfigError::invalid(NOTIFY_URL_KEY, url.redacted(), reason))?
                    .topic
            }
            None => None,
        };

        let ntfy_token = ntfy_token(reader, NTFY_TOKEN_KEY)?;
        if ntfy_token.is_some() && format != NotifyFormat::Ntfy {
            return Err(ConfigError::invalid(
                NTFY_TOKEN_KEY,
                "(hidden)",
                "only used with ntfy; set AGENT_NOTIFY_FORMAT=ntfy or unset the token",
            ));
        }

        Ok(Self {
            remote_approval: remote_approval(
                reader,
                notify_topic.as_deref(),
                ntfy_token.is_some(),
            )?,
            webhook,
            format,
            events: notify_events(reader)?,
            min_turn: Duration::from_secs(reader.parsed("AGENT_NOTIFY_MIN_SECS", 30)?),
            excerpt_chars: reader.parsed("AGENT_NOTIFY_EXCERPT_CHARS", 100)?,
            ntfy_token,
        })
    }
}

/// Remote approval, when a reply topic is set. `notify_topic` is the ntfy
/// topic notifications go to, `None` when the webhook is not ntfy.
///
/// Every requirement is checked here rather than left to fail at the first
/// prompt: a reply topic the operator set and the agent quietly ignored would
/// look like a phone that never gets buttons.
fn remote_approval(
    reader: &Reader<'_>,
    notify_topic: Option<&str>,
    has_token: bool,
) -> Result<Option<RemoteApprovalSettings>, ConfigError> {
    let reply_token = ntfy_token(reader, NTFY_REPLY_TOKEN_KEY)?;
    let Some(reply_topic) = reader.string(NTFY_REPLY_TOPIC_KEY) else {
        if reply_token.is_some() {
            return Err(ConfigError::invalid(
                NTFY_REPLY_TOKEN_KEY,
                "(hidden)",
                format!("only used for remote approval; set {NTFY_REPLY_TOPIC_KEY} as well"),
            ));
        }
        return Ok(None);
    };

    let Some(notify_topic) = notify_topic else {
        return Err(ConfigError::invalid(
            NTFY_REPLY_TOPIC_KEY,
            "(hidden)",
            format!(
                "remote approval needs ntfy notifications: set {NOTIFY_URL_KEY} to \
                 https://<ntfy server>/<topic>, with AGENT_NOTIFY_FORMAT=ntfy for a \
                 self-hosted server"
            ),
        ));
    };
    if !crate::notify::is_valid_topic(&reply_topic) {
        return Err(ConfigError::invalid(
            NTFY_REPLY_TOPIC_KEY,
            "(hidden)",
            "an ntfy topic may only contain [A-Za-z0-9_-], up to 64 characters",
        ));
    }
    if reply_topic == notify_topic {
        return Err(ConfigError::invalid(
            NTFY_REPLY_TOPIC_KEY,
            "(hidden)",
            "must differ from the notification topic, or every notification would be read \
             back as a reply",
        ));
    }
    if !has_token {
        return Err(ConfigError::missing(
            NTFY_TOKEN_KEY,
            format!(
                "Remote approval ({NTFY_REPLY_TOPIC_KEY}) needs an ntfy access token (tk_...) \
                 for a server where anonymous users can neither read the notification topic \
                 nor write to the reply topic."
            ),
        ));
    }

    let ttl: u64 = reader.parsed(NTFY_TTL_KEY, 900)?;
    if ttl == 0 {
        return Err(ConfigError::invalid(
            NTFY_TTL_KEY,
            "0",
            "a token that expires at once could never be used; e.g. 900 for 15 minutes",
        ));
    }

    Ok(Some(RemoteApprovalSettings {
        reply_topic,
        reply_token,
        ttl: Duration::from_secs(ttl),
    }))
}

/// Absent or blank means every kind: someone who set a URL and nothing else
/// wants to hear about everything.
fn notify_events(reader: &Reader<'_>) -> Result<Vec<NotificationKind>, ConfigError> {
    let entries = reader.list(NOTIFY_EVENTS_KEY);
    if entries.is_empty() {
        return Ok(NotificationKind::ALL.to_vec());
    }

    let mut kinds = entries
        .iter()
        .map(|entry| {
            NotificationKind::ALL
                .into_iter()
                .find(|kind| kind.as_str().eq_ignore_ascii_case(entry))
                .ok_or_else(|| {
                    ConfigError::invalid(
                        NOTIFY_EVENTS_KEY,
                        entry,
                        "expected a comma-separated list of: approval, completed, failed",
                    )
                })
        })
        .collect::<Result<Vec<_>, _>>()?;
    kinds.sort();
    kinds.dedup();
    Ok(kinds)
}

#[derive(Debug, Clone)]
pub struct Settings {
    pub workspace: PathBuf,
    pub approval: ApprovalPolicy,
    /// Lets changes to dependency files through without asking. Separate from
    /// `approval` on purpose: `auto` approves tool calls, and a dependency
    /// change is a different question that no policy answers by itself. For
    /// unattended runs (CI) whose operator has decided that ahead of time.
    pub allow_dependency_changes: bool,
    pub llm: LlmSettings,
    pub agent_loop: LoopSettings,
    pub subagents: SubagentSettings,
    pub session: SessionSettings,
    pub prompt: PromptSettings,
    pub web_fetch: WebFetchSettings,
    pub web_search: WebSearchSettings,
    pub shell: ShellSettings,
    pub mcp: McpSettings,
    pub hooks: HookSettings,
    /// `AGENT_PERMISSIONS`: the absolute path of the operator's permission
    /// rules, read at startup (see [`load_permissions`]). `None`: no rules,
    /// the approval policy alone decides.
    pub permissions: Option<PathBuf>,
    pub notify: NotifySettings,
    pub access: AccessSettings,
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
            allow_dependency_changes: reader.parsed("AGENT_ALLOW_DEPENDENCY_CHANGES", false)?,
            llm: LlmSettings::read(&reader)?,
            agent_loop: LoopSettings::read(&reader)?,
            subagents: SubagentSettings::read(&reader)?,
            session: SessionSettings {
                log: reader.parsed("AGENT_SESSION_LOG", true)?,
                dir: reader
                    .string("AGENT_SESSION_DIR")
                    .map_or_else(|| PathBuf::from(".agent/sessions"), PathBuf::from),
                retention: RetentionPolicy {
                    // Zero is the off switch for each half independently, so
                    // "keep everything for 30 days" and "keep the last 50
                    // forever" are both sayable.
                    keep: positive(reader.parsed("AGENT_SESSION_KEEP", 50)?),
                    max_age: positive(reader.parsed("AGENT_SESSION_MAX_AGE_DAYS", 30)?)
                        .map(|days| Duration::from_secs(days as u64 * 24 * 60 * 60)),
                },
            },
            prompt: PromptSettings::read(&reader)?,
            web_fetch: WebFetchSettings {
                enabled: reader.parsed("AGENT_WEB_FETCH", false)?,
                allowed_domains: reader.list("AGENT_WEB_FETCH_ALLOW"),
                allow_private: reader.parsed("AGENT_WEB_FETCH_ALLOW_PRIVATE", false)?,
                max_bytes: reader.parsed("AGENT_WEB_FETCH_MAX_BYTES", 1024 * 1024)?,
                timeout: Duration::from_secs(reader.parsed("AGENT_WEB_FETCH_TIMEOUT_SECS", 30)?),
            },
            web_search: WebSearchSettings::read(&reader)?,
            shell: ShellSettings {
                enabled: reader.parsed("AGENT_SHELL", false)?,
                sandbox: reader.parsed("AGENT_SHELL_SANDBOX", SandboxRequirement::default())?,
                allowed_domains: reader.list("AGENT_SHELL_NETWORK_ALLOW"),
                extra_writable: reader
                    .list("AGENT_SHELL_WRITABLE")
                    .into_iter()
                    .map(PathBuf::from)
                    .collect(),
                background: BackgroundSettings::read(&reader)?,
            },
            mcp: McpSettings::read(&reader)?,
            hooks: HookSettings::read(&reader)?,
            permissions: permissions::read_permissions_path(&reader)?,
            notify: NotifySettings::read(&reader)?,
            access: AccessSettings::read(&reader)?,
            max_file_bytes: reader.parsed("AGENT_MAX_FILE_BYTES", 2 * 1024 * 1024)?,
        })
    }

    /// The secrets the operator configured: every provider key and the parts
    /// of the webhook URL that grant access.
    ///
    /// Besides being redacted, these are withheld from child processes by
    /// value - whichever variable holds one - so that a setting such as
    /// `AGENT_NOTIFY_WEBHOOK_URL` needs no second list of names to stay out of
    /// a command's environment.
    pub fn configured_secrets(&self) -> Vec<String> {
        let mut secrets: Vec<String> = self
            .llm
            .providers
            .iter()
            .filter_map(|provider| provider.api_key.clone())
            .collect();
        if let Some(url) = &self.notify.webhook {
            secrets.extend(url.secrets(self.notify.format));
        }
        secrets.extend(self.notify.ntfy_token.as_ref().map(NtfyToken::secret));
        if let Some(remote) = &self.notify.remote_approval {
            // Knowing the reply topic is half of what it takes to post a reply.
            secrets.push(remote.reply_topic.clone());
            secrets.extend(remote.reply_token.as_ref().map(NtfyToken::secret));
        }
        // Whether or not searching is on: a key that is set is a key to keep.
        if let Some(key) = &self.web_search.api_key {
            secrets.push(key.expose().to_string());
        }
        secrets
    }

    /// Every secret value this process holds, for the redactor that scrubs
    /// tool output and notifications: [`Self::configured_secrets`], and the
    /// value of every variable in `environment` whose name the command sandbox
    /// would keep from a child process - when the value itself looks like a
    /// secret (see [`crate::secrets::secret_values`]).
    ///
    /// The environment is passed in (`SystemEnv.vars()` in production) for
    /// the same reason the settings are read through an [`EnvSource`]. It
    /// matters beyond the variables this program reads: `.env` is loaded into
    /// the process environment at startup, so whatever else it holds - a
    /// `GITHUB_TOKEN`, a database password - is in there too.
    pub fn secrets(&self, environment: impl IntoIterator<Item = (String, String)>) -> Vec<String> {
        let mut secrets = self.configured_secrets();
        secrets.extend(crate::secrets::secret_values(environment));
        secrets
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
            compact: reader.parsed("AGENT_COMPACT", true)?,
            compact_keep_recent: reader.parsed("AGENT_COMPACT_KEEP_RECENT", 12)?,
            compact_keep_recent_bytes: reader
                .parsed("AGENT_COMPACT_KEEP_RECENT_BYTES", 64 * 1024)?,
            tool_timeout: Duration::from_secs(reader.parsed("AGENT_TOOL_TIMEOUT_SECS", 60)?),
            parallel_read_only_tools: reader.parsed("AGENT_PARALLEL_READ_TOOLS", true)?,
            stream: reader.parsed("AGENT_STREAM", true)?,
            // Both opt-in. Current Claude models reject `temperature` outright,
            // so a default would break every request; and a `max_tokens`
            // default belongs to the adapter, because a size that suits the
            // Anthropic API overflows a small local model's context.
            temperature: reader.optional("AGENT_TEMPERATURE")?,
            max_tokens: reader.optional("AGENT_MAX_TOKENS")?,
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

        let routing = RoutingSettings::read(reader, &providers)?;

        Ok(Self {
            router: routing.router(reader, default_router)?,
            providers,
            default_provider,
            request_timeout: Duration::from_secs(reader.parsed("AGENT_REQUEST_TIMEOUT_SECS", 180)?),
            max_retries: reader.parsed("AGENT_MAX_RETRIES", 3)?,
            prompt_cache: reader.parsed("AGENT_PROMPT_CACHE", PromptCache::default())?,
            thinking: reader.parsed("AGENT_THINKING", Thinking::default())?,
            effort: reader.optional("AGENT_EFFORT")?,
            routing,
        })
    }
}

/// `0` reads as "no limit" for every retention setting.
///
/// A limit of zero would otherwise mean "delete everything immediately", which
/// nobody types on purpose and which destroys exactly the thing the setting is
/// about.
fn positive(value: usize) -> Option<usize> {
    (value > 0).then_some(value)
}

fn names(providers: &[ProviderSettings]) -> String {
    providers
        .iter()
        .map(|provider| provider.id.to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_domain::model::access::AccessPolicy;
    use agent_domain::model::workspace::WorkspacePath;

    fn settings(pairs: &[(&str, &str)]) -> Result<Settings, ConfigError> {
        let mut env = MapEnv::new(pairs);
        // Always set, so the tests never depend on the process' cwd.
        if env.get("AGENT_WORKSPACE").is_none() {
            env.set("AGENT_WORKSPACE", "/workspace");
        }
        // Required in the single-provider form. Supplied here so each test
        // states only what it is about; the multi-provider form names its
        // kinds explicitly.
        if env.get("AGENT_PROVIDERS").is_none() && env.get("AGENT_PROVIDER").is_none() {
            env.set("AGENT_PROVIDER", "ollama");
        }
        Settings::from_source(&env)
    }

    #[test]
    fn history_compaction_is_on_by_default() {
        // The alternative is not "no cost" but "the oldest turns are deleted",
        // and the oldest turn is where the user said what they wanted.
        let settings = settings(&[("AGENT_MODEL", "m")]).unwrap();
        assert!(settings.agent_loop.compact);
        assert_eq!(settings.agent_loop.compact_keep_recent, 12);
    }

    #[test]
    fn compaction_can_be_turned_off_for_a_metered_endpoint() {
        let settings = settings(&[
            ("AGENT_MODEL", "m"),
            ("AGENT_COMPACT", "false"),
            ("AGENT_COMPACT_KEEP_RECENT", "4"),
        ])
        .unwrap();
        assert!(!settings.agent_loop.compact);
        assert_eq!(settings.agent_loop.compact_keep_recent, 4);
    }

    #[test]
    fn the_session_log_is_on_and_lands_inside_the_workspace() {
        let settings = settings(&[("AGENT_MODEL", "m")]).unwrap();

        assert!(settings.session.log);
        assert_eq!(
            settings.session_dir(),
            PathBuf::from("/workspace/.agent/sessions"),
            "a relative setting resolves against the workspace, not the cwd"
        );
    }

    #[test]
    fn sessions_are_kept_by_count_and_by_age() {
        let settings = settings(&[("AGENT_MODEL", "m")]).unwrap();
        assert_eq!(settings.session.retention.keep, Some(50));
        assert_eq!(
            settings.session.retention.max_age,
            Some(Duration::from_secs(30 * 24 * 60 * 60))
        );
    }

    #[test]
    fn zero_means_no_limit_rather_than_delete_everything() {
        // The dangerous reading of `0`. Someone typing it means "stop deleting
        // my sessions", never "delete them all now".
        let settings = settings(&[
            ("AGENT_MODEL", "m"),
            ("AGENT_SESSION_KEEP", "0"),
            ("AGENT_SESSION_MAX_AGE_DAYS", "0"),
        ])
        .unwrap();

        assert_eq!(settings.session.retention.keep, None);
        assert_eq!(settings.session.retention.max_age, None);
        assert!(settings.session.retention.keeps_everything());
    }

    #[test]
    fn each_retention_limit_can_be_lifted_on_its_own() {
        let settings =
            settings(&[("AGENT_MODEL", "m"), ("AGENT_SESSION_MAX_AGE_DAYS", "0")]).unwrap();

        assert_eq!(settings.session.retention.keep, Some(50));
        assert_eq!(settings.session.retention.max_age, None);
        assert!(!settings.session.retention.keeps_everything());
    }

    #[test]
    fn the_session_log_can_be_turned_off_or_pointed_elsewhere() {
        // Off matters: the log holds the conversation verbatim, including the
        // contents of every file the model read.
        let off = settings(&[("AGENT_MODEL", "m"), ("AGENT_SESSION_LOG", "false")]).unwrap();
        assert!(!off.session.log);

        let elsewhere = settings(&[
            ("AGENT_MODEL", "m"),
            ("AGENT_SESSION_DIR", "/var/lib/agent"),
        ])
        .unwrap();
        assert_eq!(
            elsewhere.session_dir(),
            PathBuf::from("/var/lib/agent"),
            "an absolute setting is taken as given, so the log can be kept \
             outside the workspace the model can read"
        );
    }

    #[test]
    fn no_mcp_config_means_no_servers() {
        let settings = settings(&[("AGENT_MODEL", "m")]).unwrap();
        assert_eq!(settings.mcp.config, None);
        assert_eq!(settings.mcp.sandbox, SandboxRequirement::Confined);
    }

    #[test]
    fn the_mcp_config_must_be_an_absolute_path() {
        let error =
            settings(&[("AGENT_MODEL", "m"), ("AGENT_MCP_CONFIG", "mcp.json")]).unwrap_err();
        assert!(error.to_string().contains("AGENT_MCP_CONFIG"), "{error}");

        let settings = settings(&[
            ("AGENT_MODEL", "m"),
            ("AGENT_MCP_CONFIG", "/etc/agent/mcp.json"),
        ])
        .unwrap();
        assert_eq!(
            settings.mcp.config,
            Some(PathBuf::from("/etc/agent/mcp.json"))
        );
    }

    #[test]
    fn the_permission_rules_are_off_by_default_and_need_an_absolute_path() {
        assert_eq!(settings(&[("AGENT_MODEL", "m")]).unwrap().permissions, None);
        let error =
            settings(&[("AGENT_MODEL", "m"), ("AGENT_PERMISSIONS", "rules.json")]).unwrap_err();
        assert!(error.to_string().contains("AGENT_PERMISSIONS"), "{error}");

        let settings = settings(&[
            ("AGENT_MODEL", "m"),
            ("AGENT_PERMISSIONS", "/etc/agent/permissions.json"),
        ])
        .unwrap();
        assert_eq!(
            settings.permissions,
            Some(PathBuf::from("/etc/agent/permissions.json"))
        );
    }

    #[test]
    fn hooks_are_off_by_default_and_confined_when_on() {
        let defaults = settings(&[("AGENT_MODEL", "m")]).unwrap();
        assert_eq!(defaults.hooks.config, None);
        assert_eq!(defaults.hooks.sandbox, SandboxRequirement::Confined);

        let error = settings(&[("AGENT_MODEL", "m"), ("AGENT_HOOKS", "hooks.json")]).unwrap_err();
        assert!(error.to_string().contains("AGENT_HOOKS"), "{error}");

        let configured = settings(&[
            ("AGENT_MODEL", "m"),
            ("AGENT_HOOKS", "/etc/agent/hooks.json"),
        ])
        .unwrap();
        assert_eq!(
            configured.hooks.config,
            Some(PathBuf::from("/etc/agent/hooks.json"))
        );
    }

    #[test]
    fn mcp_servers_cannot_run_unsandboxed_even_when_commands_can() {
        let error = settings(&[
            ("AGENT_MODEL", "m"),
            ("AGENT_SHELL_SANDBOX", "none"),
            ("AGENT_MCP_SANDBOX", "none"),
        ])
        .unwrap_err();
        assert!(error.to_string().contains("AGENT_MCP_SANDBOX"), "{error}");

        // The shell's opt-out does not leak into the servers' requirement.
        let settings = settings(&[("AGENT_MODEL", "m"), ("AGENT_SHELL_SANDBOX", "none")]).unwrap();
        assert_eq!(settings.mcp.sandbox, SandboxRequirement::Confined);
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
    fn background_jobs_have_finite_limits_by_default() {
        let background = settings(&[("AGENT_MODEL", "m")]).unwrap().shell.background;
        assert!(background.enabled);
        assert_eq!(background.max_jobs, 4);
        assert_eq!(background.max_lifetime, Duration::from_secs(30 * 60));
        assert_eq!(background.buffer_bytes, 256 * 1024);
    }

    #[test]
    fn a_zero_background_limit_is_refused_rather_than_read_as_off() {
        for key in [
            "AGENT_SHELL_BACKGROUND_MAX_JOBS",
            "AGENT_SHELL_BACKGROUND_LIFETIME_SECS",
            "AGENT_SHELL_BACKGROUND_BUFFER_BYTES",
        ] {
            let error =
                settings(&[("AGENT_MODEL", "m"), (key, "0")]).expect_err("zero must abort startup");
            assert!(error.to_string().contains(key), "{error}");
        }
        let background = settings(&[
            ("AGENT_MODEL", "m"),
            ("AGENT_SHELL_BACKGROUND", "false"),
            ("AGENT_SHELL_BACKGROUND_LIFETIME_SECS", "90"),
        ])
        .unwrap()
        .shell
        .background;
        assert!(!background.enabled);
        assert_eq!(background.max_lifetime, Duration::from_secs(90));
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
    fn subagents_are_on_with_their_own_bounds_unless_turned_off() {
        let default = settings(&[("AGENT_MODEL", "m")]).unwrap().subagents;
        assert_eq!(
            default,
            SubagentSettings {
                enabled: true,
                max_iterations: 15,
                max_history_bytes: 128 * 1024,
                max_answer_bytes: 8 * 1024,
            }
        );

        let tuned = settings(&[
            ("AGENT_MODEL", "m"),
            ("AGENT_SUBAGENTS", "false"),
            ("AGENT_SUBAGENT_MAX_ITERATIONS", "0"),
            ("AGENT_SUBAGENT_MAX_HISTORY_BYTES", "4096"),
            ("AGENT_SUBAGENT_MAX_ANSWER_BYTES", "512"),
        ])
        .unwrap()
        .subagents;
        assert!(!tuned.enabled);
        assert_eq!(
            tuned.max_iterations, 1,
            "a zero budget still allows one request"
        );
        assert_eq!(tuned.max_history_bytes, 4096);
        assert_eq!(tuned.max_answer_bytes, 512);

        let error = settings(&[("AGENT_MODEL", "m"), ("AGENT_SUBAGENTS", "maybe")])
            .expect_err("an unreadable switch must abort startup");
        assert!(error.to_string().contains("AGENT_SUBAGENTS"), "{error}");
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
    fn dependency_changes_are_asked_about_unless_explicitly_allowed() {
        let default = settings(&[("AGENT_MODEL", "m"), ("AGENT_APPROVAL", "auto")]).unwrap();
        assert!(
            !default.allow_dependency_changes,
            "`auto` alone must not let dependency changes through"
        );

        let allowed = settings(&[
            ("AGENT_MODEL", "m"),
            ("AGENT_ALLOW_DEPENDENCY_CHANGES", "true"),
        ])
        .unwrap();
        assert!(allowed.allow_dependency_changes);
        assert!(describe(&allowed)["dependency changes"].contains("ALLOWED"));
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
            ("AGENT_PROVIDER_LOCAL_KIND", "ollama"),
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
            ("AGENT_PROVIDER_LOCAL_KIND", "ollama"),
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
    fn prompt_caching_is_on_with_a_five_minute_ttl_unless_told_otherwise() {
        let default = settings(&[("AGENT_MODEL", "m")]).unwrap();
        assert_eq!(default.llm.prompt_cache, PromptCache::FiveMinutes);
        assert_eq!(
            describe(&default)["prompt cache"],
            "5m (anthropic providers)"
        );

        let hour = settings(&[("AGENT_MODEL", "m"), ("AGENT_PROMPT_CACHE", "1h")]).unwrap();
        assert_eq!(hour.llm.prompt_cache, PromptCache::OneHour);

        let off = settings(&[("AGENT_MODEL", "m"), ("AGENT_PROMPT_CACHE", "off")]).unwrap();
        assert_eq!(off.llm.prompt_cache, PromptCache::Off);
        assert_eq!(describe(&off)["prompt cache"], "disabled");

        let error = settings(&[("AGENT_MODEL", "m"), ("AGENT_PROMPT_CACHE", "1d")])
            .expect_err("an unknown TTL must abort startup");
        assert!(error.to_string().contains("AGENT_PROMPT_CACHE"), "{error}");
        assert!(error.to_string().contains("5m|1h|off"), "{error}");
    }

    #[test]
    fn thinking_is_off_and_effort_unsent_unless_configured() {
        let default = settings(&[("AGENT_MODEL", "m")]).unwrap();
        assert_eq!(default.llm.thinking, Thinking::Off);
        assert_eq!(default.llm.effort, None);
        assert_eq!(
            describe(&default)["thinking"],
            "off; effort not sent (anthropic providers)"
        );

        let set = settings(&[
            ("AGENT_MODEL", "m"),
            ("AGENT_THINKING", "adaptive"),
            ("AGENT_EFFORT", "xhigh"),
        ])
        .unwrap();
        assert_eq!(set.llm.thinking, Thinking::Adaptive);
        assert_eq!(set.llm.effort, Some(Effort::XHigh));
        assert_eq!(
            describe(&set)["thinking"],
            "adaptive; effort xhigh (anthropic providers)"
        );

        for (key, value, alternatives) in [
            ("AGENT_THINKING", "disabled", "off|adaptive"),
            ("AGENT_EFFORT", "extreme", "low|medium|high|xhigh|max"),
        ] {
            let error = settings(&[("AGENT_MODEL", "m"), (key, value)])
                .expect_err("an unknown value must abort startup");
            assert!(error.to_string().contains(key), "{error}");
            assert!(error.to_string().contains(alternatives), "{error}");
        }
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
            ("AGENT_MAX_TOKENS", "32000"),
        ])
        .unwrap();

        assert_eq!(settings.agent_loop.max_iterations, 3);
        assert_eq!(settings.agent_loop.tool_timeout, Duration::from_secs(7));
        assert!(!settings.agent_loop.parallel_read_only_tools);
        assert!(!settings.agent_loop.stream);
        assert_eq!(settings.agent_loop.temperature, Some(0.9));
        assert_eq!(settings.agent_loop.max_tokens, Some(32000));
    }

    #[test]
    fn temperature_and_max_tokens_are_unset_unless_configured() {
        let settings = settings(&[("AGENT_MODEL", "m")]).unwrap();

        assert_eq!(
            settings.agent_loop.temperature, None,
            "a default would be sent, and current Claude models reject it"
        );
        assert_eq!(
            settings.agent_loop.max_tokens, None,
            "the default belongs to each adapter, not to the configuration"
        );
        assert_eq!(
            describe(&settings)["generation"],
            "temperature not sent; max_tokens 16000 (anthropic) / server default (openai)"
        );
    }

    #[test]
    fn doctor_warns_when_a_temperature_will_reach_an_anthropic_provider() {
        let anthropic = settings(&[
            ("AGENT_PROVIDER", "anthropic"),
            ("AGENT_API_KEY", "sk-ant-test"),
            ("AGENT_MODEL", "claude-sonnet-5-5"),
            ("AGENT_TEMPERATURE", "0.5"),
        ])
        .unwrap();
        let rendered = &describe(&anthropic)["generation"];
        assert!(
            rendered.starts_with("temperature 0.5 (WARNING"),
            "{rendered}"
        );
        assert!(rendered.contains("AGENT_TEMPERATURE"), "{rendered}");

        let local = settings(&[("AGENT_MODEL", "m"), ("AGENT_TEMPERATURE", "0.5")]).unwrap();
        assert!(
            !describe(&local)["generation"].contains("WARNING"),
            "OpenAI-compatible servers accept a temperature"
        );

        let bad = settings(&[("AGENT_MODEL", "m"), ("AGENT_TEMPERATURE", "warm")])
            .expect_err("an unparsable value must abort startup, not fall back");
        assert!(bad.to_string().contains("AGENT_TEMPERATURE"), "{bad}");
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
        assert_eq!(settings.prompt.global_instructions, None);
        assert_eq!(describe(&settings)["global instructions"], "none");
    }

    #[test]
    fn global_instructions_are_read_from_an_absolute_path() {
        let settings = settings(&[
            ("AGENT_MODEL", "m"),
            (
                "AGENT_GLOBAL_INSTRUCTIONS",
                "/home/me/.config/agent/AGENTS.md",
            ),
        ])
        .unwrap();
        assert_eq!(
            settings.prompt.global_instructions,
            Some(PathBuf::from("/home/me/.config/agent/AGENTS.md"))
        );
        assert_eq!(
            describe(&settings)["global instructions"],
            "/home/me/.config/agent/AGENTS.md"
        );
    }

    #[test]
    fn a_relative_global_instructions_path_aborts_startup_and_says_why() {
        for relative in ["AGENTS.md", "~/.config/agent/AGENTS.md"] {
            let error = settings(&[
                ("AGENT_MODEL", "m"),
                ("AGENT_GLOBAL_INSTRUCTIONS", relative),
            ])
            .err()
            .unwrap_or_else(|| panic!("`{relative}` must be refused"));
            let message = error.to_string();
            assert!(
                message.contains("AGENT_GLOBAL_INSTRUCTIONS") && message.contains("absolute"),
                "{message}"
            );
        }
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

    #[test]
    fn notifications_are_off_and_subscribe_to_everything_by_default() {
        let settings = settings(&[("AGENT_MODEL", "m")]).unwrap();

        assert!(settings.notify.webhook.is_none());
        assert_eq!(settings.notify.format, NotifyFormat::Json);
        assert_eq!(settings.notify.events, NotificationKind::ALL.to_vec());
        assert_eq!(settings.notify.min_turn, Duration::from_secs(30));
        assert_eq!(settings.notify.excerpt_chars, 100);
        assert_eq!(describe(&settings)["notify"], "disabled");
    }

    #[test]
    fn the_public_ntfy_host_implies_the_ntfy_format_unless_told_otherwise() {
        let inferred = settings(&[
            ("AGENT_MODEL", "m"),
            ("AGENT_NOTIFY_WEBHOOK_URL", "https://ntfy.sh/my-topic"),
        ])
        .unwrap();
        assert_eq!(inferred.notify.format, NotifyFormat::Ntfy);

        let explicit = settings(&[
            ("AGENT_MODEL", "m"),
            ("AGENT_NOTIFY_WEBHOOK_URL", "https://ntfy.sh/my-topic"),
            ("AGENT_NOTIFY_FORMAT", "json"),
        ])
        .unwrap();
        assert_eq!(explicit.notify.format, NotifyFormat::Json);
    }

    #[test]
    fn notification_filters_and_limits_come_from_the_environment() {
        let settings = settings(&[
            ("AGENT_MODEL", "m"),
            (
                "AGENT_NOTIFY_WEBHOOK_URL",
                "https://hooks.slack.com/services/T/B/x",
            ),
            ("AGENT_NOTIFY_FORMAT", "slack"),
            ("AGENT_NOTIFY_EVENTS", "failed, Approval,failed"),
            ("AGENT_NOTIFY_MIN_SECS", "0"),
            ("AGENT_NOTIFY_EXCERPT_CHARS", "0"),
        ])
        .unwrap();

        assert_eq!(settings.notify.format, NotifyFormat::Slack);
        assert_eq!(
            settings.notify.events,
            vec![NotificationKind::Approval, NotificationKind::Failed]
        );
        assert_eq!(settings.notify.min_turn, Duration::ZERO);
        assert_eq!(settings.notify.excerpt_chars, 0);
    }

    #[test]
    fn a_bad_notification_setting_aborts_startup_and_names_the_variable() {
        for (key, value) in [
            ("AGENT_NOTIFY_FORMAT", "ntfyy"),
            ("AGENT_NOTIFY_EVENTS", "approval,done"),
            ("AGENT_NOTIFY_MIN_SECS", "soon"),
        ] {
            let error = settings(&[
                ("AGENT_MODEL", "m"),
                ("AGENT_NOTIFY_WEBHOOK_URL", "https://example.com/hook"),
                (key, value),
            ])
            .expect_err("startup must fail");
            assert!(error.to_string().contains(key), "{error}");
        }
    }

    #[test]
    fn a_bad_webhook_url_is_rejected_without_being_repeated() {
        let error = settings(&[
            ("AGENT_MODEL", "m"),
            ("AGENT_NOTIFY_WEBHOOK_URL", "hooks.example.com/s3cr3t"),
        ])
        .expect_err("not an absolute URL");
        let message = error.to_string();
        assert!(message.contains("AGENT_NOTIFY_WEBHOOK_URL"), "{message}");
        assert!(!message.contains("s3cr3t"), "{message}");

        let error = settings(&[
            ("AGENT_MODEL", "m"),
            ("AGENT_NOTIFY_WEBHOOK_URL", "https://ntfy.sh/"),
            ("AGENT_NOTIFY_FORMAT", "ntfy"),
        ])
        .expect_err("ntfy without a topic");
        assert!(error.to_string().contains("topic"), "{error}");
    }

    const NTFY_TOKEN: &str = "tk_0123456789abcdefghijklmnopq";

    fn remote(extra: &[(&str, &str)]) -> Result<Settings, ConfigError> {
        let mut pairs = vec![
            ("AGENT_MODEL", "m"),
            (
                "AGENT_NOTIFY_WEBHOOK_URL",
                "https://ntfy.example.com/alerts-42",
            ),
            ("AGENT_NOTIFY_FORMAT", "ntfy"),
            ("AGENT_NOTIFY_NTFY_TOKEN", NTFY_TOKEN),
            ("AGENT_NOTIFY_NTFY_REPLY_TOPIC", "replies-from-phone"),
        ];
        pairs.retain(|(key, _)| !extra.iter().any(|(extra, _)| extra == key));
        pairs.extend(extra.iter().filter(|(_, value)| !value.is_empty()));
        settings(&pairs)
    }

    #[test]
    fn remote_approval_is_off_unless_a_reply_topic_is_set() {
        let settings = settings(&[
            ("AGENT_MODEL", "m"),
            ("AGENT_NOTIFY_WEBHOOK_URL", "https://ntfy.sh/alerts-42"),
        ])
        .unwrap();
        assert!(settings.notify.remote_approval.is_none());
        assert!(describe(&settings)["remote approval"].starts_with("disabled"));
    }

    #[test]
    fn remote_approval_reads_its_topic_tokens_and_lifetime() {
        let settings = remote(&[
            (
                "AGENT_NOTIFY_NTFY_REPLY_TOKEN",
                "tk_replyreplyreplyreplyreply",
            ),
            ("AGENT_NOTIFY_APPROVAL_TTL_SECS", "120"),
        ])
        .unwrap();
        let remote = settings.notify.remote_approval.as_ref().unwrap();
        assert_eq!(remote.reply_topic, "replies-from-phone");
        assert!(remote.reply_token.is_some());
        assert_eq!(remote.ttl, Duration::from_secs(120));

        let default = self::remote(&[]).unwrap();
        assert_eq!(
            default.notify.remote_approval.unwrap().ttl,
            Duration::from_secs(900)
        );
    }

    #[test]
    fn remote_approval_without_a_token_or_without_ntfy_aborts_startup() {
        let error = remote(&[("AGENT_NOTIFY_NTFY_TOKEN", "")]).expect_err("no token");
        assert!(
            error.to_string().contains("AGENT_NOTIFY_NTFY_TOKEN"),
            "{error}"
        );

        let error = remote(&[
            ("AGENT_NOTIFY_FORMAT", "json"),
            ("AGENT_NOTIFY_NTFY_TOKEN", ""),
        ])
        .expect_err("not ntfy");
        assert!(error.to_string().contains("ntfy"), "{error}");

        let error =
            remote(&[("AGENT_NOTIFY_NTFY_REPLY_TOPIC", "alerts-42")]).expect_err("same topic");
        assert!(error.to_string().contains("differ"), "{error}");

        let error =
            remote(&[("AGENT_NOTIFY_NTFY_REPLY_TOPIC", "bad.topic")]).expect_err("invalid topic");
        assert!(!error.to_string().contains("bad.topic"), "{error}");

        let error = remote(&[("AGENT_NOTIFY_APPROVAL_TTL_SECS", "0")]).expect_err("zero ttl");
        assert!(
            error.to_string().contains("AGENT_NOTIFY_APPROVAL_TTL_SECS"),
            "{error}"
        );
    }

    #[test]
    fn ntfy_tokens_and_the_reply_topic_are_secrets_and_never_printed() {
        let settings = remote(&[(
            "AGENT_NOTIFY_NTFY_REPLY_TOKEN",
            "tk_replyreplyreplyreplyreply",
        )])
        .unwrap();

        let secrets = settings.configured_secrets();
        for secret in [
            NTFY_TOKEN,
            "tk_replyreplyreplyreplyreply",
            "replies-from-phone",
        ] {
            assert!(secrets.iter().any(|s| s == secret), "{secret}: {secrets:?}");
        }

        let shown = format!("{settings:?} {:?}", describe(&settings));
        for secret in [NTFY_TOKEN, "tk_reply", "replies-from-phone"] {
            assert!(!shown.contains(secret), "`{secret}` leaked: {shown}");
        }
    }

    #[test]
    fn the_webhook_url_is_masked_in_debug_output_and_in_doctor() {
        let settings = settings(&[
            ("AGENT_MODEL", "m"),
            (
                "AGENT_NOTIFY_WEBHOOK_URL",
                "https://hooks.slack.com/services/T000/B000/XXXXSECRET",
            ),
        ])
        .unwrap();

        let debug = format!("{settings:?}");
        let doctor = describe(&settings)["notify"].clone();
        for shown in [&debug, &doctor] {
            assert!(!shown.contains("XXXXSECRET"), "{shown}");
            assert!(!shown.contains("services"), "{shown}");
        }
        assert!(
            doctor.contains("https://hooks.slack.com/…"),
            "doctor still says where notifications go: {doctor}"
        );
    }

    // --- file access ---------------------------------------------------------

    use agent_domain::model::access::FileAccess;

    fn readable(policy: &AccessPolicy, path: &str) -> bool {
        policy.permits(&WorkspacePath::parse(path).unwrap(), FileAccess::Read)
    }

    #[test]
    fn the_built_in_rules_and_the_session_log_are_protected_by_default() {
        let settings = settings(&[("AGENT_MODEL", "m")]).unwrap();
        let policy = settings.access_policy(Path::new("/workspace")).unwrap();

        assert!(!readable(&policy, ".env"));
        assert!(!readable(&policy, ".agent/sessions/0b1c.jsonl"));
        assert!(readable(&policy, ".agent/instructions.md"));
        assert!(readable(&policy, "src/main.rs"));
    }

    #[test]
    fn the_session_log_is_protected_wherever_inside_the_workspace_it_is() {
        for dir in [
            "/workspace/logs",
            "logs",
            "tmp/../logs",
            "/workspace/./logs/",
        ] {
            let settings = settings(&[("AGENT_MODEL", "m"), ("AGENT_SESSION_DIR", dir)]).unwrap();
            let policy = settings.access_policy(Path::new("/workspace")).unwrap();
            assert!(
                !readable(&policy, "logs/0b1c.jsonl"),
                "AGENT_SESSION_DIR={dir} left the log readable: {}",
                policy.describe()
            );
        }
    }

    #[test]
    fn a_session_log_outside_the_workspace_adds_no_rule() {
        let settings = settings(&[
            ("AGENT_MODEL", "m"),
            ("AGENT_SESSION_DIR", "/var/lib/agent"),
        ])
        .unwrap();
        assert_eq!(
            settings.access_policy(Path::new("/workspace")).unwrap(),
            AccessPolicy::standard()
        );
    }

    #[test]
    fn the_session_log_cannot_be_the_workspace_root() {
        for dir in [".", "/workspace"] {
            let settings = settings(&[("AGENT_MODEL", "m"), ("AGENT_SESSION_DIR", dir)]).unwrap();
            let error = settings
                .access_policy(Path::new("/workspace"))
                .expect_err("startup must fail rather than refuse every file");
            assert!(error.to_string().contains("AGENT_SESSION_DIR"), "{error}");
        }
    }

    #[test]
    fn operator_rules_extend_the_built_in_ones() {
        let settings = settings(&[
            ("AGENT_MODEL", "m"),
            ("AGENT_DENY_READ", "*.p12, secrets/ ,"),
            ("AGENT_DENY_WRITE", "Cargo.lock"),
        ])
        .unwrap();
        assert_eq!(
            settings.access.deny_read.len(),
            2,
            "blank entries are dropped"
        );

        let policy = settings.access_policy(Path::new("/workspace")).unwrap();
        assert!(!readable(&policy, "certs/client.p12"));
        assert!(!readable(&policy, "deploy/secrets/prod.yml"));
        assert!(readable(&policy, "Cargo.lock"));
        assert!(!policy.permits(
            &WorkspacePath::parse("Cargo.lock").unwrap(),
            FileAccess::Write
        ));
        assert!(!readable(&policy, ".env"), "the built-in rules stay");
    }

    #[test]
    fn an_unusable_rule_aborts_startup_and_names_the_variable() {
        for (key, value) in [
            ("AGENT_DENY_READ", "!.env.local"),
            ("AGENT_DENY_READ", "/etc/shadow"),
            ("AGENT_DENY_WRITE", "**/*.lock"),
            ("AGENT_DENY_WRITE", "secret?.txt"),
        ] {
            let error = settings(&[("AGENT_MODEL", "m"), (key, value)])
                .expect_err("a rule that cannot be enforced as written must not be skipped");
            let message = error.to_string();
            assert!(message.contains(key), "{message}");
            assert!(message.contains(value), "{message}");
        }
    }

    #[test]
    fn doctor_lists_every_protected_path_on_one_line() {
        let settings = settings(&[("AGENT_MODEL", "m"), ("AGENT_DENY_READ", "*.p12")]).unwrap();
        let shown = describe(&settings)["file access"].clone();

        assert!(!shown.contains('\n'), "{shown}");
        for rule in [
            ".env,",
            ".env.example",
            ".agent/sessions/",
            "*.p12",
            ".git/",
        ] {
            assert!(shown.contains(rule), "`{rule}` is missing: {shown}");
        }
    }

    #[test]
    fn secrets_cover_every_provider_key_and_the_webhook() {
        let settings = settings(&[
            ("AGENT_PROVIDERS", "local,cloud"),
            ("AGENT_PROVIDER_LOCAL_KIND", "openai"),
            ("AGENT_PROVIDER_LOCAL_MODEL", "m"),
            ("AGENT_PROVIDER_LOCAL_API_KEY", "sk-proj-LOCALKEY123"),
            ("AGENT_PROVIDER_CLOUD_KIND", "anthropic"),
            ("AGENT_PROVIDER_CLOUD_MODEL", "claude-sonnet-5"),
            ("AGENT_PROVIDER_CLOUD_API_KEY", "sk-ant-CLOUDKEY456"),
            (
                "AGENT_NOTIFY_WEBHOOK_URL",
                "https://ntfy.sh/private-topic-789",
            ),
        ])
        .unwrap();

        let redactor = crate::tools::Redactor::new(settings.secrets(Vec::new()));
        let leaked = "keys sk-proj-LOCALKEY123 sk-ant-CLOUDKEY456, topic private-topic-789";
        let shown = redactor.redact(leaked);
        for secret in ["LOCALKEY", "CLOUDKEY", "private-topic"] {
            assert!(!shown.contains(secret), "`{secret}` survived: {shown}");
        }
    }

    #[test]
    fn a_configured_key_is_redacted_but_a_guessed_dictionary_word_is_not() {
        let settings = settings(&[
            ("AGENT_MODEL", "m"),
            // Short and plain, but the operator said it is a key.
            ("AGENT_API_KEY", "abcdefgh"),
        ])
        .unwrap();
        let environment = [
            ("POSTGRES_PASSWORD", "postgres"),
            ("OPENAI_API_KEY", "sk-proj-AbC123dEf456"),
        ]
        .map(|(name, value)| (name.to_string(), value.to_string()));

        let redactor = crate::tools::Redactor::new(settings.secrets(environment));
        assert_eq!(
            redactor.redact("key abcdefgh, db postgres, other sk-proj-AbC123dEf456"),
            "key [REDACTED], db postgres, other [REDACTED]",
        );
    }

    #[test]
    fn configured_secrets_hold_the_webhook_but_nothing_from_the_environment() {
        let settings = settings(&[
            ("AGENT_MODEL", "m"),
            (
                "AGENT_NOTIFY_WEBHOOK_URL",
                "https://ntfy.sh/private-topic-789",
            ),
        ])
        .unwrap();
        let configured = settings.configured_secrets();
        assert!(
            configured.contains(&"https://ntfy.sh/private-topic-789".to_string()),
            "{configured:?}"
        );
    }

    #[test]
    fn secrets_include_every_variable_the_command_sandbox_would_withhold() {
        let settings = settings(&[("AGENT_MODEL", "m")]).unwrap();
        let environment = [
            ("GITHUB_TOKEN", "ghp_0123456789abcdef"),
            ("db_password", "correct-horse-battery"),
            ("NPM_TOKEN", "short"),
            ("PATH", "/usr/local/bin:/usr/bin"),
            ("AGENT_WORKSPACE", "/workspace/project"),
        ]
        .map(|(name, value)| (name.to_string(), value.to_string()));

        let redactor = crate::tools::Redactor::new(settings.secrets(environment));
        let shown = redactor.redact(
            "ghp_0123456789abcdef correct-horse-battery short /usr/local/bin:/usr/bin \
             /workspace/project",
        );
        assert_eq!(
            shown, "[REDACTED] [REDACTED] short /usr/local/bin:/usr/bin /workspace/project",
            "secret-named values are hidden, others and short ones are not"
        );
    }

    // --- web search -----------------------------------------------------------

    const SEARCH_KEY: &str = "BSA-search-key-0123456789";

    #[test]
    fn web_search_is_off_by_default() {
        let settings = settings(&[("AGENT_MODEL", "m")]).unwrap();
        assert!(!settings.web_search.enabled);
        assert!(settings.web_search.active_key().is_none());
        assert_eq!(describe(&settings)["web search"], "disabled");
    }

    #[test]
    fn enabling_web_search_without_a_key_fails_and_names_the_variable() {
        let error = settings(&[("AGENT_MODEL", "m"), ("AGENT_WEB_SEARCH", "true")]).unwrap_err();
        let message = error.to_string();
        assert!(message.contains("AGENT_WEB_SEARCH_API_KEY"), "{message}");
    }

    #[test]
    fn the_search_key_is_a_configured_secret_and_never_printed() {
        let settings = settings(&[
            ("AGENT_MODEL", "m"),
            ("AGENT_WEB_SEARCH", "true"),
            ("AGENT_WEB_SEARCH_API_KEY", SEARCH_KEY),
        ])
        .unwrap();

        assert_eq!(
            settings.web_search.active_key().map(SearchApiKey::expose),
            Some(SEARCH_KEY)
        );
        assert!(
            settings
                .configured_secrets()
                .contains(&SEARCH_KEY.to_string())
        );
        assert!(!format!("{settings:?}").contains(SEARCH_KEY));
        let doctor = describe(&settings)["web search"].clone();
        assert!(doctor.starts_with("enabled"), "{doctor}");
        assert!(!doctor.contains(SEARCH_KEY), "{doctor}");
    }

    #[test]
    fn a_key_alone_does_not_enable_web_search() {
        let settings = settings(&[
            ("AGENT_MODEL", "m"),
            ("AGENT_WEB_SEARCH_API_KEY", SEARCH_KEY),
        ])
        .unwrap();
        assert!(settings.web_search.active_key().is_none());
        assert!(
            settings
                .configured_secrets()
                .contains(&SEARCH_KEY.to_string()),
            "a key that is set is withheld whether or not it is used"
        );
    }
}
