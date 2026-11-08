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
pub mod routes;

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use crate::session::store::RetentionPolicy;

use agent_domain::error::DomainError;
use agent_domain::model::access::{AccessPolicy, PathPattern};
use agent_domain::model::llm::{ModelId, ProviderId};
use agent_domain::model::workspace::{WorkspacePath, WorkspaceRoot};
use agent_domain::ports::notify::NotificationKind;
use thiserror::Error;

use crate::exec::SandboxRequirement;
use crate::llm::PromptCache;
use crate::notify::{NotifyFormat, WebhookUrl};

pub use env::{EnvSource, MapEnv, SystemEnv};
pub use kinds::{ApprovalPolicy, ProviderKind, RouterKind};
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
}

const NOTIFY_URL_KEY: &str = "AGENT_NOTIFY_WEBHOOK_URL";
const NOTIFY_EVENTS_KEY: &str = "AGENT_NOTIFY_EVENTS";

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
        if let Some(url) = &webhook {
            crate::notify::endpoint(url, format)
                .map_err(|reason| ConfigError::invalid(NOTIFY_URL_KEY, url.redacted(), reason))?;
        }

        Ok(Self {
            webhook,
            format,
            events: notify_events(reader)?,
            min_turn: Duration::from_secs(reader.parsed("AGENT_NOTIFY_MIN_SECS", 30)?),
            excerpt_chars: reader.parsed("AGENT_NOTIFY_EXCERPT_CHARS", 100)?,
        })
    }
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

/// Files inside the workspace the file tools refuse, on top of the built-in
/// rules ([`AccessPolicy::standard`]) and the session log directory.
///
/// Additions only. There is deliberately no setting that removes a built-in
/// rule: those name files that are never project files, and a project that
/// needs a value from one can put it in the prompt.
#[derive(Debug, Clone, Default)]
pub struct AccessSettings {
    /// Neither read nor written.
    pub deny_read: Vec<PathPattern>,
    /// Readable, but never written.
    pub deny_write: Vec<PathPattern>,
}

const DENY_READ_KEY: &str = "AGENT_DENY_READ";
const DENY_WRITE_KEY: &str = "AGENT_DENY_WRITE";

impl AccessSettings {
    fn read(reader: &Reader<'_>) -> Result<Self, ConfigError> {
        Ok(Self {
            deny_read: patterns(reader, DENY_READ_KEY)?,
            deny_write: patterns(reader, DENY_WRITE_KEY)?,
        })
    }
}

/// A pattern that does not parse aborts startup. Skipping it would leave the
/// operator believing a file is protected that is not.
fn patterns(reader: &Reader<'_>, key: &str) -> Result<Vec<PathPattern>, ConfigError> {
    reader
        .list(key)
        .iter()
        .map(|raw| {
            PathPattern::parse(raw).map_err(|error| {
                let reason = match error {
                    DomainError::Invalid { reason, .. } => reason,
                    other => other.to_string(),
                };
                ConfigError::invalid(
                    key,
                    raw,
                    format!(
                        "{reason}. Expected names such as `*.p12` or directories such as `secrets/`"
                    ),
                )
            })
        })
        .collect()
}

#[derive(Debug, Clone)]
pub struct Settings {
    pub workspace: PathBuf,
    pub approval: ApprovalPolicy,
    pub llm: LlmSettings,
    pub agent_loop: LoopSettings,
    pub session: SessionSettings,
    pub prompt: PromptSettings,
    pub web_fetch: WebFetchSettings,
    pub shell: ShellSettings,
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
            llm: LlmSettings::read(&reader)?,
            agent_loop: LoopSettings::read(&reader)?,
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
            notify: NotifySettings::read(&reader)?,
            access: AccessSettings::read(&reader)?,
            max_file_bytes: reader.parsed("AGENT_MAX_FILE_BYTES", 2 * 1024 * 1024)?,
        })
    }

    /// The rules the file tools enforce in a workspace at `workspace`.
    ///
    /// The workspace is passed in rather than read from `self.workspace`
    /// because the composition root has the canonical path, and the session
    /// directory has to be compared with the path files are really reached
    /// by.
    pub fn access_policy(&self, workspace: &Path) -> Result<AccessPolicy, ConfigError> {
        let mut policy = AccessPolicy::standard();

        // Protected whether or not logging is on: turning the log off stops
        // new records, it does not remove the old ones.
        if let Some(sessions) = self.session_dir_inside(workspace) {
            let pattern = PathPattern::directory(&sessions).map_err(|_| {
                ConfigError::invalid(
                    "AGENT_SESSION_DIR",
                    self.session.dir.display().to_string(),
                    "the session log cannot be the workspace root itself: it holds the whole \
                     conversation, and keeping it from the file tools would refuse every file. \
                     Use a subdirectory such as .agent/sessions, or a path outside the workspace",
                )
            })?;
            policy = policy.with_read_denied(pattern);
        }

        for pattern in &self.access.deny_read {
            policy = policy.with_read_denied(pattern.clone());
        }
        for pattern in &self.access.deny_write {
            policy = policy.with_write_denied(pattern.clone());
        }
        Ok(policy)
    }

    /// Where the session log sits relative to `workspace`, or `None` when it
    /// is outside.
    ///
    /// Compared both as written and as resolved, so that neither a `..` in
    /// the setting nor a symlink between the two spellings of the workspace
    /// hides that the log is inside.
    fn session_dir_inside(&self, workspace: &Path) -> Option<WorkspacePath> {
        let dir = if self.session.dir.is_absolute() {
            self.session.dir.clone()
        } else {
            workspace.join(&self.session.dir)
        };
        let spellings = |path: &Path| {
            [Some(lexically_normal(path)), resolved(path)]
                .into_iter()
                .flatten()
        };

        spellings(workspace)
            .filter_map(|root| WorkspaceRoot::new(root).ok())
            .find_map(|root| spellings(&dir).find_map(|dir| root.relativize(&dir).ok()))
    }

    /// Every secret value this process holds, for the redactor that scrubs
    /// tool output and notifications: the provider keys, the webhook URL, and
    /// the value of every variable in `environment` whose name the command
    /// sandbox would keep from a child process.
    ///
    /// The environment is passed in (`SystemEnv.vars()` in production) for
    /// the same reason the settings are read through an [`EnvSource`]. It
    /// matters beyond the variables this program reads: `.env` is loaded into
    /// the process environment at startup, so whatever else it holds - a
    /// `GITHUB_TOKEN`, a database password - is in there too.
    pub fn secrets(&self, environment: impl IntoIterator<Item = (String, String)>) -> Vec<String> {
        let mut secrets: Vec<String> = self
            .llm
            .providers
            .iter()
            .filter_map(|provider| provider.api_key.clone())
            .collect();
        if let Some(url) = &self.notify.webhook {
            secrets.extend(url.secrets(self.notify.format));
        }
        secrets.extend(crate::exec::env::secret_values(environment));
        secrets
    }

    /// Absolute location of the session directory.
    ///
    /// A relative setting is read against the workspace rather than against
    /// the current directory, so `agent` started from a subdirectory writes to
    /// the same place as one started from the root.
    pub fn session_dir(&self) -> PathBuf {
        if self.session.dir.is_absolute() {
            self.session.dir.clone()
        } else {
            self.workspace.join(&self.session.dir)
        }
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

        let routing = RoutingSettings::read(reader, &providers)?;

        Ok(Self {
            router: routing.router(reader, default_router)?,
            providers,
            default_provider,
            request_timeout: Duration::from_secs(reader.parsed("AGENT_REQUEST_TIMEOUT_SECS", 180)?),
            max_retries: reader.parsed("AGENT_MAX_RETRIES", 3)?,
            prompt_cache: reader.parsed("AGENT_PROMPT_CACHE", PromptCache::default())?,
            routing,
        })
    }
}

/// Says what will be deleted, in the words of the settings that decide it.
///
/// `doctor` is where someone checks this before trusting the machine with a
/// long session, so "kept forever" has to be as plain as the limits are.
fn describe_retention(policy: &RetentionPolicy) -> String {
    let count = policy
        .keep
        .map(|keep| format!("newest {keep}"))
        .unwrap_or_else(|| "any number".to_string());
    let age = policy
        .max_age
        .map(|max_age| format!("{} days", max_age.as_secs() / (24 * 60 * 60)))
        .unwrap_or_else(|| "forever".to_string());

    format!("{count}, kept {age}")
}

/// Where notifications go and what they carry, with the URL reduced to its
/// host. The excerpt is called out because it is the part that leaves the
/// machine with the model's words in it.
fn describe_notify(notify: &NotifySettings) -> String {
    let Some(url) = &notify.webhook else {
        return "disabled".to_string();
    };
    let events = notify
        .events
        .iter()
        .map(|kind| kind.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    let excerpt = if notify.excerpt_chars == 0 {
        "no answer excerpt".to_string()
    } else {
        format!("first {} chars of each answer", notify.excerpt_chars)
    };
    format!(
        "{} to {} ({events}; turns >= {}s; {excerpt})",
        notify.format.as_str(),
        url.redacted(),
        notify.min_turn.as_secs()
    )
}

/// `path` with `.` and `..` resolved by text alone. `..` above the root
/// stays at the root, as it does on the filesystem.
fn lexically_normal(path: &Path) -> PathBuf {
    let mut normal = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normal.pop();
            }
            other => normal.push(other),
        }
    }
    normal
}

/// `path` with symlinks resolved, including when it does not exist yet - the
/// session directory is only created by the first session. The deepest
/// existing ancestor is resolved and the rest appended as written.
fn resolved(path: &Path) -> Option<PathBuf> {
    let path = lexically_normal(path);
    let mut existing = path.as_path();
    while std::fs::symlink_metadata(existing).is_err() {
        existing = existing.parent()?;
    }
    let tail = path.strip_prefix(existing).ok()?;
    Some(std::fs::canonicalize(existing).ok()?.join(tail))
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
    description.insert("notify".into(), describe_notify(&settings.notify));
    description.insert(
        "file access".into(),
        match settings.access_policy(&settings.workspace) {
            Ok(policy) => policy.describe(),
            Err(error) => error.to_string(),
        },
    );
    description.insert(
        "session log".into(),
        if settings.session.log {
            format!(
                "{} (holds the conversation verbatim)",
                settings.session_dir().display()
            )
        } else {
            "disabled".to_string()
        },
    );
    description.insert(
        "session retention".into(),
        describe_retention(&settings.session.retention),
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
    description.insert(
        "history".into(),
        format!(
            "{} bytes; {}",
            settings.agent_loop.max_history_bytes,
            if settings.agent_loop.compact {
                format!(
                    "compacted into a summary, keeping {} recent messages",
                    settings.agent_loop.compact_keep_recent
                )
            } else {
                "oldest turns dropped".to_string()
            }
        ),
    );
    description.insert("router".into(), settings.llm.router.as_str().into());
    description.extend(settings.llm.routing.describe());
    description.insert(
        "prompt cache".into(),
        match settings.llm.prompt_cache {
            PromptCache::Off => "disabled".to_string(),
            ttl => format!("{} (anthropic providers)", ttl.as_str()),
        },
    );
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
}
