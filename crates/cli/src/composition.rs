//! The composition root.
//!
//! This is the only place in the program where concrete types meet the ports
//! they implement. Every layer above receives `Arc<dyn Trait>` and therefore
//! stays swappable - which is what makes "point the agent at Ollama instead of
//! Anthropic", "auto-approve in CI", or "add a router" configuration changes
//! rather than code changes.

use std::sync::Arc;

use agent_application::agent::{
    AgentDependencies, AgentLoop, AgentLoopConfig, AppendingPromptBuilder, DefaultPromptBuilder,
    FixedPromptBuilder,
};
use agent_application::checkpoint::{CheckpointJournal, CheckpointingTool};
use agent_application::tools::ToolRegistry;
use agent_application::tools::exec::RunCommandTool;
use agent_application::tools::file::{
    EditFileTool, GlobTool, ListDirectoryTool, ReadFileTool, SearchFilesTool, WriteFileTool,
};
use agent_application::tools::plan::TodoWriteTool;
use agent_application::tools::server::server_tools;
use agent_application::tools::web::{WebFetchTool, WebSearchTool};
use agent_domain::model::llm::{GenerationParams, ModelId};
use agent_domain::model::tool::ToolSafety;
use agent_domain::model::workspace::WorkspaceRoot;
use agent_domain::ports::approval::ApprovalGate;
use agent_domain::ports::conversation_log::{ConversationLog, NullConversationLog};
use agent_domain::ports::events::EventSink;
use agent_domain::ports::file_system::FileSystem;
use agent_domain::ports::llm::LlmProvider;
use agent_domain::ports::notify::{Notifier, NullNotifier};
use agent_domain::ports::prompt::PromptBuilder;
use agent_domain::ports::remote_approval::{NullRemoteApprover, RemoteApprover};
use agent_domain::ports::session_store::SessionStore;
use agent_domain::ports::tool::Tool;
use agent_domain::ports::tool_server::ToolServer;
use agent_infrastructure::config::{
    ApprovalPolicy, NotifySettings, PromptSettings, Settings, ShellSettings, SystemEnv,
};
use agent_infrastructure::exec::{ExecConfig, SandboxedCommandRunner};
use agent_infrastructure::fs::{IgnoreAwareSearcher, LocalFileSystem, WorkspaceContextProvider};
use agent_infrastructure::llm::build_provider;
use agent_infrastructure::mcp::{McpConfig, McpServer, start_servers};
use agent_infrastructure::notify::{NtfyRemoteApprover, WebhookNotifier};
use agent_infrastructure::session::{FileConversationLog, FileSessionStore};
use agent_infrastructure::tools::{DependencyWatchTool, RedactingTool, Redactor, TimeoutTool};
use agent_infrastructure::web::{BraveSearcher, GuardedWebFetcher, WebFetchConfig};
use anyhow::{Context, Result};

use tracing::warn;

use crate::approval::CliApprovalGate;
use crate::args::{Cli, Resume};
use crate::notify::{Notifications, NotifyPolicy};
use crate::render::TerminalRenderer;

pub struct Application {
    pub settings: Settings,
    pub workspace: Arc<WorkspaceRoot>,
    pub tools: Arc<ToolRegistry>,
    pub provider: Arc<dyn LlmProvider>,
    pub agent: AgentLoop,
    /// Reads back what the log wrote. Present even when logging is off, in
    /// which case it simply finds nothing - `agent sessions` should say "no
    /// sessions" rather than fail.
    pub sessions: Arc<dyn SessionStore>,
    /// Kept alive for the life of the process: it owns the egress proxy's
    /// port, and the commands it confines refer to that port by number.
    /// `doctor` also reads its reported sandbox rather than the configuration.
    pub commands: Option<Arc<SandboxedCommandRunner>>,
    /// The same instance the approval gate holds: the gate restarts the turn
    /// clock when a prompt is answered, and the subcommands read it.
    pub notifications: Arc<Notifications>,
    /// What the approval gate races against the terminal. Active only when
    /// remote approval is configured, the session is interactive, and the
    /// server passed the startup checks - `doctor` reports which.
    pub remote_approval: Arc<dyn RemoteApprover>,
    /// The MCP servers, running for the life of the process. Their tools hold
    /// them too; dropping the last handle stops the server and its group.
    pub tool_servers: Vec<Arc<McpServer>>,
    /// The one redactor tool output and notifications go through, for
    /// anything else the process writes out that may echo a secret (an eval
    /// report quoting a check's output).
    pub redactor: Arc<Redactor>,
    /// What `/undo` and `/rewind` take back. The loop holds it too; `chat`
    /// clears it when the conversation starts over.
    pub checkpoints: Arc<CheckpointJournal>,
    /// The tools' file system, for `@path` references in `chat`: what the
    /// user attaches is read under the same rules as what the model reads.
    pub files: Arc<dyn FileSystem>,
}

/// What differs between the callers of [`assemble`].
///
/// Everything else - the workspace, where the session log goes, which tools
/// are offered, the approval policy - is in the [`Settings`] passed alongside,
/// so a caller that wants a different workspace or log directory (a scratch
/// workspace per eval task, a temporary one in a test) sets
/// `settings.workspace` / `settings.session.dir` rather than a second copy
/// here that could disagree with them.
pub struct Wiring {
    /// Whether a human can answer an approval prompt. Without one the gate
    /// denies rather than waiting forever.
    pub interactive: bool,
    /// Where the loop narrates what it does.
    pub events: Arc<dyn EventSink>,
    /// Overrides the provider's model on every request (`--model`).
    pub model: Option<ModelId>,
    /// Environment variables whose secret-looking values are redacted from
    /// tool output and notifications, on top of the keys in `Settings`.
    pub environment: Vec<(String, String)>,
    /// `Some` prunes old session records at startup, sparing the one this
    /// resume names; `None` leaves the session directory alone.
    pub retention: Option<Resume>,
}

/// The agent as `agent` itself runs it: settings from the environment with
/// the command-line flags on top, narrated to the terminal.
///
/// Async because the egress proxy binds a socket at startup - see
/// [`SandboxedCommandRunner::start`].
pub async fn build(cli: &Cli, interactive: bool) -> Result<Application> {
    let events = Arc::new(TerminalRenderer::new(cli.verbose, cli.no_color));
    build_with_events(cli, interactive, events).await
}

/// [`build`], narrated to `events` instead of the terminal - for
/// `agent run --output-format json`, whose stdout must carry JSON only.
pub async fn build_with_events(
    cli: &Cli,
    interactive: bool,
    events: Arc<dyn EventSink>,
) -> Result<Application> {
    let settings = resolve_settings(cli)?;
    assemble(
        settings,
        Wiring {
            interactive,
            events,
            model: cli.model.as_deref().map(ModelId::new),
            // Read after `main` loaded `.env`, so its variables are among the
            // environment's.
            environment: SystemEnv.vars(),
            retention: Some(cli.resolve_command().resume()),
        },
    )
    .await
}

/// Wires the whole agent from resolved settings.
///
/// The one place every subcommand's agent is built, so that what a test or
/// an eval run exercises is the wiring `agent run` uses, not a copy of it.
pub async fn assemble(settings: Settings, wiring: Wiring) -> Result<Application> {
    let Wiring {
        interactive,
        events,
        model,
        environment,
        retention,
    } = wiring;

    // --- workspace sandbox ---------------------------------------------------
    let canonical = std::fs::canonicalize(&settings.workspace).with_context(|| {
        format!(
            "workspace `{}` does not exist or is not readable",
            settings.workspace.display()
        )
    })?;
    // Built against the canonical path, which is what the session directory
    // has to be compared with.
    let access = settings
        .access_policy(&canonical)
        .context("invalid file access settings")?;
    let workspace = Arc::new(
        WorkspaceRoot::new(canonical)
            .context("the workspace path must be absolute")?
            .with_policy(access),
    );
    // Read before the redactor: what the operator hands a server is a
    // credential, and the server can echo it back.
    let mcp_config = load_mcp_config(&settings, workspace.as_path(), &environment)?;
    // Kept from every child's environment by value. A server's `env_from`
    // secrets are in this process's environment, so `run_command` and the
    // other servers would inherit them otherwise.
    let withheld: Vec<String> = settings
        .configured_secrets()
        .into_iter()
        .chain(mcp_config.iter().flat_map(McpConfig::inherited_values))
        .collect();
    // One instance for tool output and for notifications: a secret hidden
    // from the model must not leave by the other door.
    let redactor = Arc::new(Redactor::new(
        settings
            .secrets(environment)
            .into_iter()
            .chain(mcp_config.iter().flat_map(McpConfig::secret_values)),
    ));

    // --- adapters ------------------------------------------------------------
    let file_system = Arc::new(
        LocalFileSystem::new(workspace.clone(), settings.max_file_bytes)
            .context("cannot open the workspace")?,
    );
    let searcher = Arc::new(IgnoreAwareSearcher::new(workspace.clone()));
    // Checked here rather than when the prompt is built: an operator who set
    // the variable expects the file in every session, and a typo found only
    // as a log line would quietly run without it - the same reason a missing
    // AGENT_SYSTEM_PROMPT_FILE aborts startup.
    if let Some(path) = &settings.prompt.global_instructions {
        anyhow::ensure!(
            path.is_file(),
            "AGENT_GLOBAL_INSTRUCTIONS points at `{}`, which is not a readable file. Fix the \
             path or unset the variable",
            path.display()
        );
    }
    let context = Arc::new(
        WorkspaceContextProvider::new(workspace.clone())
            .with_global_instructions(settings.prompt.global_instructions.clone()),
    );
    let provider = build_provider(&settings.llm).context("cannot build the LLM provider")?;

    // No URL means `NullNotifier`, not a flag: nothing above this line has a
    // "notifications are off" branch, and nothing is ever sent.
    let notifier: Arc<dyn Notifier> = match webhook(&settings.notify)? {
        Some(webhook) => Arc::new(webhook),
        None => Arc::new(NullNotifier),
    };
    let notifications = Arc::new(
        Notifications::new(notifier, NotifyPolicy::from_settings(&settings.notify))
            .with_redactor(redactor.clone()),
    );

    let remote_approval = remote_approver(&settings.notify, interactive).await?;
    let approval: Arc<dyn ApprovalGate> = Arc::new(
        CliApprovalGate::new(settings.approval, interactive, notifications.clone())
            .with_dependency_changes_allowed(settings.allow_dependency_changes)
            .with_remote(remote_approval.clone()),
    );

    let checkpoints = Arc::new(CheckpointJournal::new(file_system.clone()));
    let files: Arc<dyn FileSystem> = file_system.clone();
    let mut tools = build_tools(
        file_system,
        searcher,
        workspace.clone(),
        settings.agent_loop.tool_timeout,
    );

    // web_fetch is opt-in: registering it at all is the operator's decision
    // (AGENT_WEB_FETCH=true), because a URL is an outbound message.
    if settings.web_fetch.enabled {
        let fetcher = GuardedWebFetcher::new(WebFetchConfig {
            allowed_domains: settings.web_fetch.allowed_domains.clone(),
            allow_private: settings.web_fetch.allow_private,
            max_bytes: settings.web_fetch.max_bytes,
            timeout: settings.web_fetch.timeout,
        })
        .context("cannot build the web fetcher")?;

        tools.register(TimeoutTool::wrap(
            Arc::new(WebFetchTool::new(Arc::new(fetcher))),
            settings.agent_loop.tool_timeout,
        ));
    }

    // web_search likewise (AGENT_WEB_SEARCH=true): a query is an outbound
    // message. The settings refuse to enable it without a key.
    if let Some(key) = settings.web_search.active_key() {
        let searcher = BraveSearcher::new(key.clone()).context("cannot build the web searcher")?;
        tools.register(TimeoutTool::wrap(
            Arc::new(WebSearchTool::new(Arc::new(searcher))),
            settings.agent_loop.tool_timeout,
        ));
    }

    // run_command is opt-in for a stronger reason than web_fetch: a command's
    // effect cannot be read off its arguments. Startup fails outright when the
    // sandbox is weaker than asked for, rather than registering the tool with
    // less protection than the operator believes it has.
    let commands = if settings.shell.enabled {
        let runner =
            build_command_runner(&settings.shell, workspace.clone(), withheld.clone()).await?;

        tools.register(TimeoutTool::wrap(
            Arc::new(RunCommandTool::new(
                runner.clone(),
                &settings.shell.allowed_domains,
            )),
            // The command has its own timeout; this outer one only catches a
            // runner that never returns at all.
            settings.agent_loop.tool_timeout + std::time::Duration::from_secs(600),
        ));
        Some(runner)
    } else {
        None
    };

    // Last of the registrations, so a server tool whose name collides with
    // any tool above is refused. Before the wrapping below, so server tools
    // are watched and redacted like every other destructive tool.
    let tool_servers = register_tool_servers(
        &mut tools,
        mcp_config.as_ref(),
        &settings,
        workspace.as_path(),
        &withheld,
    )
    .await?;

    // After every tool is registered, so no command or server tool is
    // missing from what `/undo` says it did not take back.
    let tools = checkpoint_writes(tools, &checkpoints);
    // After every tool is registered, so none can be missed. Unconditional:
    // AGENT_ALLOW_DEPENDENCY_CHANGES skips the question, not the report.
    let tools = watch_destructive(tools, &workspace, &events);
    // Last, so that a tool registered above can never be missed - and so the
    // dependency note is redacted like the rest of the output.
    let tools = Arc::new(redact_all(tools, &redactor));

    // --- the loop ------------------------------------------------------------
    // Disabled means `NullConversationLog`, not a flag the loop checks: there
    // is no "logging is off" branch anywhere above this line.
    let log: Arc<dyn ConversationLog> = if settings.session.log {
        Arc::new(FileConversationLog::new(settings.session_dir()))
    } else {
        Arc::new(NullConversationLog)
    };
    // The reader is wired unconditionally: turning the log off stops new
    // sessions being written, it does not make the old ones unreadable.
    let store = Arc::new(FileSessionStore::new(settings.session_dir()));
    if let Some(resume) = &retention {
        apply_retention(&settings, &store, resume).await;
    }
    let sessions: Arc<dyn SessionStore> = store;

    let prompt = build_prompt_builder(&settings.prompt)?;
    let agent = AgentLoop::new(
        AgentDependencies {
            llm: provider.clone(),
            tools: tools.clone(),
            approval,
            events,
            context,
            prompt,
            log,
        },
        AgentLoopConfig {
            model,
            params: GenerationParams {
                temperature: settings.agent_loop.temperature,
                max_tokens: settings.agent_loop.max_tokens,
                top_p: None,
                stop_sequences: Vec::new(),
            },
            max_iterations: settings.agent_loop.max_iterations,
            max_tool_output_bytes: settings.agent_loop.max_tool_output_bytes,
            max_history_bytes: settings.agent_loop.max_history_bytes,
            keep_recent_messages: 6,
            compact: settings.agent_loop.compact,
            compact_keep_recent: settings.agent_loop.compact_keep_recent,
            compact_keep_recent_bytes: settings.agent_loop.compact_keep_recent_bytes,
            parallel_read_only_tools: settings.agent_loop.parallel_read_only_tools,
            stream: settings.agent_loop.stream,
        },
    )
    .with_checkpoints(checkpoints.clone());

    Ok(Application {
        settings,
        workspace,
        tools,
        provider,
        agent,
        sessions,
        commands,
        notifications,
        remote_approval,
        tool_servers,
        redactor,
        checkpoints,
        files,
    })
}

/// The MCP configuration, when `AGENT_MCP_CONFIG` names one. Checked against
/// the canonical workspace: neither the file nor the programs it starts may
/// live inside it. `env_from` is resolved against `environment`, the agent's
/// own variables.
fn load_mcp_config(
    settings: &Settings,
    workspace: &std::path::Path,
    environment: &[(String, String)],
) -> Result<Option<McpConfig>> {
    let lookup = |name: &str| {
        environment
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.clone())
    };
    settings
        .mcp
        .config
        .as_ref()
        .map(|path| {
            McpConfig::load(path, workspace, &lookup)
                .context("AGENT_MCP_CONFIG cannot be used. Fix the file, or unset the variable")
        })
        .transpose()
}

/// Starts the configured MCP servers and registers their tools, each in a
/// timeout. Nothing at all without a configuration.
///
/// Every failure is fatal - a server that does not start, a sandbox the
/// platform lacks, a tool whose name would collide - because an agent quietly
/// missing tools the operator configured, or quietly running them with less
/// confinement, is worse than one that does not start.
async fn register_tool_servers(
    registry: &mut ToolRegistry,
    config: Option<&McpConfig>,
    settings: &Settings,
    workspace: &std::path::Path,
    withheld: &[String],
) -> Result<Vec<Arc<McpServer>>> {
    let Some(config) = config else {
        return Ok(Vec::new());
    };
    let servers = start_servers(config, settings.mcp.sandbox, workspace, withheld)
        .await
        .context(
            "cannot start the MCP servers AGENT_MCP_CONFIG lists. They run only under a sandbox \
             (Linux 6.7+ for Landlock; macOS is not supported yet) - unset AGENT_MCP_CONFIG to \
             start without them",
        )?;
    for server in &servers {
        let tools = server_tools(server.clone() as Arc<dyn ToolServer>, registry)?;
        for tool in tools {
            registry.register(TimeoutTool::wrap(tool, settings.agent_loop.tool_timeout));
        }
    }
    Ok(servers)
}

/// Deletes the session records the retention policy no longer covers.
///
/// Startup is the natural place for it: the log only ever appends, so
/// something has to be the thing that removes, and there is no other moment
/// that happens on its own. It costs one directory read and one `stat` per
/// file - no session is opened - so it does not grow into the startup path.
///
/// The session `--resume` names is exempt. Reaching for an old record by id is
/// the normal reason to have one, and deleting it in the second before opening
/// it would be the worst possible timing.
///
/// A failure here is a warning, never a refusal to start. Housekeeping that
/// could not run is not a reason to withhold the agent.
async fn apply_retention(settings: &Settings, store: &FileSessionStore, resume: &Resume) {
    if settings.session.retention.keeps_everything() {
        return;
    }

    let protected: Vec<String> = match resume {
        Resume::No => Vec::new(),
        Resume::Session(id) => vec![id.clone()],
        // `keep` alone would spare the newest, but `max_age` would not: a
        // session resumed after a long break is old by definition.
        Resume::Latest => store.latest().await.ok().flatten().into_iter().collect(),
    };
    let protected: Vec<&str> = protected.iter().map(String::as_str).collect();

    match store.prune(&settings.session.retention, &protected).await {
        Ok(removed) if !removed.is_empty() => eprintln!(
            "removed {} old session record(s); `agent doctor` shows the retention policy",
            removed.len()
        ),
        Ok(_) => {}
        Err(error) => warn!(%error, "cannot apply the session retention policy"),
    }
}

/// Builds the command runner, turning a sandbox shortfall into an error the
/// operator can act on.
///
/// The hint matters more than it looks: the failure is nearly always "this
/// kernel is older than 6.7" or "this is macOS, where the mechanism is not
/// written yet", and an operator who only sees "sandbox unavailable" will
/// reach for the off switch rather than for the fix.
async fn build_command_runner(
    shell: &ShellSettings,
    workspace: Arc<WorkspaceRoot>,
    withheld: Vec<String>,
) -> Result<Arc<SandboxedCommandRunner>> {
    let runner = SandboxedCommandRunner::start(
        workspace,
        ExecConfig {
            sandbox: shell.sandbox,
            extra_writable: shell.extra_writable.clone(),
            allowed_domains: shell.allowed_domains.clone(),
            withheld,
        },
    )
    .await
    .with_context(|| {
        "cannot run commands under the sandbox AGENT_SHELL_SANDBOX asks for. Either use a \
         platform that supports it (Linux 6.7+ for Landlock), or set AGENT_SHELL=false to \
         drop the tool. AGENT_SHELL_SANDBOX=none disables the sandbox itself and is only \
         safe in a container you already treat as disposable."
    })?;

    Ok(Arc::new(runner))
}

/// The webhook notifications go to, if the operator configured one.
///
/// Separate from where `NullNotifier` stands in for it so that "no URL, no
/// sender" is checkable: behind `Arc<dyn Notifier>` the two look the same.
fn webhook(settings: &NotifySettings) -> Result<Option<WebhookNotifier>> {
    let Some(url) = &settings.webhook else {
        return Ok(None);
    };
    let webhook = WebhookNotifier::new(
        url,
        settings.format,
        agent_infrastructure::notify::DEFAULT_TIMEOUT,
    )
    .context("cannot set up webhook notifications")?
    .with_token(settings.ntfy_token.clone());
    Ok(Some(webhook))
}

/// Remote approval, when it is configured and could ever be used.
///
/// A session that is not interactive denies every prompt without asking
/// (`agent run` without a terminal, `eval`), so there is nothing to answer
/// from the phone and nothing is checked or sent.
///
/// Otherwise the server must pass the startup checks or the agent does not
/// start: a reply topic the operator set, quietly dropped because the server
/// let strangers read the buttons, would be a second way to approve that is
/// weaker than the operator believes.
async fn remote_approver(
    settings: &NotifySettings,
    interactive: bool,
) -> Result<Arc<dyn RemoteApprover>> {
    if !interactive || settings.remote_approval.is_none() {
        return Ok(Arc::new(NullRemoteApprover));
    }
    let approver = NtfyRemoteApprover::connect(settings).await.context(
        "cannot start remote approval (AGENT_NOTIFY_NTFY_REPLY_TOPIC). Unset it to answer \
         prompts at the terminal only",
    )?;
    let Some(approver) = approver else {
        return Ok(Arc::new(NullRemoteApprover));
    };

    let shares_token = settings.remote_approval.as_ref().is_some_and(|remote| {
        remote.reply_token.is_none() || remote.reply_token == settings.ntfy_token
    });
    if shares_token {
        warn!(
            "the approve/deny buttons carry AGENT_NOTIFY_NTFY_TOKEN, so anyone who reads a \
             notification gets everything that token can do. Set AGENT_NOTIFY_NTFY_REPLY_TOKEN \
             to a token that can only write to the reply topic"
        );
    }
    Ok(Arc::new(approver))
}

/// The default toolset, every tool wrapped in a timeout so that one
/// pathological call cannot stall the loop.
fn build_tools(
    file_system: Arc<LocalFileSystem>,
    searcher: Arc<IgnoreAwareSearcher>,
    workspace: Arc<WorkspaceRoot>,
    timeout: std::time::Duration,
) -> ToolRegistry {
    let timed = |tool: Arc<dyn Tool>| TimeoutTool::wrap(tool, timeout);
    ToolRegistry::new()
        .with(timed(Arc::new(ReadFileTool::new(
            file_system.clone(),
            workspace.clone(),
        ))))
        .with(timed(Arc::new(WriteFileTool::new(
            file_system.clone(),
            workspace.clone(),
        ))))
        .with(timed(Arc::new(EditFileTool::new(
            file_system.clone(),
            workspace.clone(),
        ))))
        .with(timed(Arc::new(ListDirectoryTool::new(
            file_system,
            workspace.clone(),
        ))))
        .with(timed(Arc::new(SearchFilesTool::new(
            searcher.clone(),
            workspace.clone(),
        ))))
        .with(timed(Arc::new(GlobTool::new(searcher, workspace))))
        .with(timed(Arc::new(TodoWriteTool::new())))
}

/// The tools `/undo` has to know about: the two that write files, whose
/// writes it takes back, and every destructive one, whose calls it lists as
/// not taken back. Outside the timeout, so a write it cuts off is still
/// recorded.
fn checkpoint_writes(tools: ToolRegistry, journal: &Arc<CheckpointJournal>) -> ToolRegistry {
    tools.map(|definition, tool| {
        let writes_files = matches!(definition.name.as_str(), "write_file" | "edit_file");
        if writes_files || definition.safety == ToolSafety::Destructive {
            CheckpointingTool::wrap(tool, journal.clone())
        } else {
            tool
        }
    })
}

/// Every destructive tool, wrapped so that a change it makes to dependency
/// files is reported. Chosen by safety rather than by name (see
/// [`DependencyWatchTool::watches`]), and wrapped outside the timeout so a
/// call cut off by it is still compared.
fn watch_destructive(
    tools: ToolRegistry,
    workspace: &Arc<WorkspaceRoot>,
    events: &Arc<dyn EventSink>,
) -> ToolRegistry {
    tools.map(|definition, tool| {
        if DependencyWatchTool::watches(definition) {
            DependencyWatchTool::wrap(tool, workspace.clone(), events.clone())
        } else {
            tool
        }
    })
}

/// Every registered tool, wrapped so that a known secret never reaches the
/// model through its output. Outermost, so it also covers the timeout's
/// error.
fn redact_all(tools: ToolRegistry, redactor: &Arc<Redactor>) -> ToolRegistry {
    tools.map(|_, tool| RedactingTool::wrap(tool, redactor.clone()))
}

/// Resolves the operator's prompt configuration into the builder the loop
/// receives.
///
/// The replacement file is read here, once, by the operator's own process -
/// not through the model's tools - so a path outside the workspace is
/// legitimate and the workspace sandbox does not apply. A path that cannot be
/// read (or an effectively empty file) aborts startup: silently falling back
/// to the built-in prompt would hide exactly the misconfiguration the
/// operator most needs to see.
pub(crate) fn build_prompt_builder(prompt: &PromptSettings) -> Result<Arc<dyn PromptBuilder>> {
    let base: Arc<dyn PromptBuilder> = match &prompt.replace_file {
        Some(path) => {
            let contents = std::fs::read_to_string(path).with_context(|| {
                format!("cannot read the system prompt file `{}`", path.display())
            })?;
            if contents.trim().is_empty() {
                anyhow::bail!(
                    "the system prompt file `{}` is empty - remove \
                     AGENT_SYSTEM_PROMPT_FILE / --system-prompt-file to use the built-in prompt",
                    path.display()
                );
            }
            Arc::new(FixedPromptBuilder::new(contents))
        }
        None => Arc::new(DefaultPromptBuilder),
    };

    Ok(match &prompt.append {
        Some(extra) => Arc::new(AppendingPromptBuilder::new(base, extra.clone())),
        None => base,
    })
}

/// Environment first, command-line flags on top.
pub(crate) fn resolve_settings(cli: &Cli) -> Result<Settings> {
    let mut settings = Settings::from_env()
        .context("invalid configuration - copy .env.example to .env (make env) and fill it in")?;
    apply_cli_overrides(&mut settings, cli);
    Ok(settings)
}

/// Command-line flags win over the environment. Split out so the precedence
/// is testable without touching the process environment.
fn apply_cli_overrides(settings: &mut Settings, cli: &Cli) {
    if let Some(workspace) = &cli.workspace {
        settings.workspace = workspace.clone();
    }
    if let Some(max_iterations) = cli.max_iterations {
        settings.agent_loop.max_iterations = max_iterations.max(1);
    }
    if cli.yes {
        settings.approval = ApprovalPolicy::Auto;
    }
    if let Some(file) = &cli.system_prompt_file {
        settings.prompt.replace_file = Some(file.clone());
    }
    if let Some(text) = &cli.append_system_prompt {
        settings.prompt.append = Some(text.clone());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_application::build_system_prompt;
    use agent_domain::model::context::ContextSnapshot;
    use clap::Parser as _;

    fn settings(pairs: &[(&str, &str)]) -> Settings {
        let mut env = agent_infrastructure::config::MapEnv::new(&[
            ("AGENT_WORKSPACE", "/workspace"),
            ("AGENT_PROVIDER", "ollama"),
            ("AGENT_MODEL", "m"),
        ]);
        for (key, value) in pairs {
            env.set(key, value);
        }
        Settings::from_source(&env).unwrap()
    }

    #[test]
    fn without_a_webhook_url_no_sender_is_built() {
        let settings = settings(&[]);
        assert!(
            webhook(&settings.notify).unwrap().is_none(),
            "no URL must mean NullNotifier, so nothing can be sent"
        );
    }

    #[test]
    fn a_webhook_url_builds_the_sender() {
        let settings = settings(&[("AGENT_NOTIFY_WEBHOOK_URL", "https://ntfy.sh/my-agent-topic")]);
        assert!(webhook(&settings.notify).unwrap().is_some());
    }

    fn snapshot() -> ContextSnapshot {
        ContextSnapshot {
            workspace_root: "/workspace".into(),
            os: "linux".into(),
            today: "2026-08-14".into(),
            is_git_repository: true,
            project_instructions: Some("Project rules here.".into()),
            nested_instructions: Vec::new(),
            directory_overview: vec!["crates/".into()],
        }
    }

    #[test]
    fn the_default_wiring_is_byte_identical_to_the_builtin_prompt() {
        let builder = build_prompt_builder(&PromptSettings::default()).unwrap();
        assert_eq!(
            builder.build(&snapshot(), &[]),
            build_system_prompt(&snapshot(), &[]),
            "with nothing injected, the sent prompt must not change by a byte"
        );
    }

    #[test]
    fn a_missing_prompt_file_aborts_startup_with_the_path_in_the_error() {
        let error = build_prompt_builder(&PromptSettings {
            replace_file: Some("/nonexistent/prompt.md".into()),
            append: None,
            global_instructions: None,
        })
        .err()
        .expect("startup must fail");
        assert!(
            format!("{error:#}").contains("/nonexistent/prompt.md"),
            "got: {error:#}"
        );
    }

    #[test]
    fn an_empty_prompt_file_is_rejected_rather_than_silently_ignored() {
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(file.path(), "  \n\n").unwrap();

        let error = build_prompt_builder(&PromptSettings {
            replace_file: Some(file.path().to_path_buf()),
            append: None,
            global_instructions: None,
        })
        .err()
        .expect("startup must fail");
        assert!(format!("{error:#}").contains("empty"), "got: {error:#}");
    }

    #[test]
    fn replacement_and_append_compose() {
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(file.path(), "You are a terse bot.\n").unwrap();

        let builder = build_prompt_builder(&PromptSettings {
            replace_file: Some(file.path().to_path_buf()),
            append: Some("Reply in Japanese.".into()),
            global_instructions: None,
        })
        .unwrap();

        let prompt = builder.build(&snapshot(), &[]);
        assert_eq!(prompt, "You are a terse bot.\n\nReply in Japanese.\n");
        assert!(
            !prompt.contains("Project rules here."),
            "a replaced prompt carries none of the default sections"
        );
    }

    /// `preview` has no default, so every decorator must forward it. One that
    /// answered "no writes" instead would let every manifest edit through it
    /// skip the dependency approval; this runs the real wrapping.
    #[tokio::test]
    async fn a_write_through_every_decorator_still_previews_the_manifest_change() {
        use agent_domain::model::tool::ToolName;
        use agent_domain::ports::events::NullEventSink;

        let dir = tempfile::tempdir().unwrap();
        let canonical = std::fs::canonicalize(dir.path()).unwrap();
        std::fs::write(canonical.join("Cargo.toml"), "[dependencies]\n").unwrap();
        let workspace = Arc::new(WorkspaceRoot::new(canonical).unwrap());
        let file_system = Arc::new(LocalFileSystem::new(workspace.clone(), 1024 * 1024).unwrap());
        let searcher = Arc::new(IgnoreAwareSearcher::new(workspace.clone()));
        let events: Arc<dyn EventSink> = Arc::new(NullEventSink);

        let tools = build_tools(
            file_system,
            searcher,
            workspace.clone(),
            std::time::Duration::from_secs(5),
        );
        let tools = watch_destructive(tools, &workspace, &events);
        let tools = redact_all(tools, &Arc::new(Redactor::new(Vec::new())));

        let write = tools.get(&ToolName::new("write_file").unwrap()).unwrap();
        let planned = write
            .preview(&serde_json::json!({
                "path": "Cargo.toml",
                "content": "[dependencies]\nserde = \"1\"\n",
            }))
            .await
            .unwrap();

        assert_eq!(planned.len(), 1, "{planned:?}");
        assert_eq!(planned[0].path.display(), "Cargo.toml");
        assert_eq!(planned[0].before.text(), Some("[dependencies]\n"));
        assert!(planned[0].after.contains("serde"));
    }

    #[tokio::test]
    async fn without_an_mcp_config_no_server_starts_and_the_registry_is_untouched() {
        use agent_infrastructure::config::MapEnv;

        let settings = Settings::from_source(&MapEnv::new(&[
            ("AGENT_WORKSPACE", "/workspace"),
            ("AGENT_PROVIDER", "ollama"),
            ("AGENT_MODEL", "m"),
        ]))
        .unwrap();
        assert!(
            load_mcp_config(&settings, std::path::Path::new("/workspace"), &[])
                .unwrap()
                .is_none()
        );

        let mut registry = ToolRegistry::new().with(Arc::new(TodoWriteTool::new()));
        let before = registry.names();
        let servers = register_tool_servers(
            &mut registry,
            None,
            &settings,
            std::path::Path::new("/workspace"),
            &[],
        )
        .await
        .unwrap();

        assert!(servers.is_empty());
        assert_eq!(registry.names(), before);
    }

    #[test]
    fn cli_flags_override_the_environment() {
        use agent_infrastructure::config::MapEnv;

        let env = MapEnv::new(&[
            ("AGENT_WORKSPACE", "/workspace"),
            ("AGENT_PROVIDER", "ollama"),
            ("AGENT_MODEL", "m"),
            ("AGENT_SYSTEM_PROMPT_FILE", "/from-env.md"),
            ("AGENT_APPEND_SYSTEM_PROMPT", "from env"),
        ]);
        let mut settings = Settings::from_source(&env).unwrap();

        let cli = Cli::parse_from([
            "agent",
            "--system-prompt-file",
            "/from-cli.md",
            "--append-system-prompt",
            "from cli",
            "run",
            "hi",
        ]);
        apply_cli_overrides(&mut settings, &cli);

        assert_eq!(
            settings.prompt.replace_file,
            Some(std::path::PathBuf::from("/from-cli.md"))
        );
        assert_eq!(settings.prompt.append.as_deref(), Some("from cli"));
    }
}
