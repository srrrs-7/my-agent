//! The binary's half of the composition root.
//!
//! The agent itself is wired by [`agent_sdk::unstable::assemble`], the same
//! function an embedder's `agent_sdk::Agent` goes through. What is left here
//! is what belongs to a terminal: the approval prompt ([`CliApprovalGate`]),
//! the push notifications it shares with the subcommands, remote approval,
//! the renderer, and the command-line flags laid over the environment.

use std::sync::Arc;

use agent_application::agent::AgentLoop;
use agent_application::approval::PlanMode;
use agent_application::checkpoint::CheckpointJournal;
use agent_application::tools::ToolRegistry;
use agent_domain::model::llm::ModelId;
use agent_domain::model::workspace::WorkspaceRoot;
use agent_domain::ports::approval::ApprovalGate;
use agent_domain::ports::events::EventSink;
use agent_domain::ports::file_system::FileSystem;
use agent_domain::ports::llm::LlmProvider;
use agent_domain::ports::notify::{Notifier, NullNotifier};
use agent_domain::ports::remote_approval::{NullRemoteApprover, RemoteApprover};
use agent_domain::ports::session_store::SessionStore;
use agent_infrastructure::config::{ApprovalPolicy, NotifySettings, Settings, SystemEnv};
use agent_infrastructure::exec::SandboxedCommandRunner;
use agent_infrastructure::exec::background::BackgroundSupervisor;
use agent_infrastructure::hooks::Hooks;
use agent_infrastructure::mcp::McpServer;
use agent_infrastructure::notify::{NtfyRemoteApprover, WebhookNotifier};
use agent_infrastructure::tools::Redactor;
use agent_sdk::unstable::{self, Assembly, GateContext, Spare};
use anyhow::{Context, Result};

use tracing::warn;

use crate::approval::CliApprovalGate;
use crate::args::{Cli, Resume};
use crate::notify::{Notifications, NotifyPolicy};
use crate::render::TerminalRenderer;

pub(crate) use agent_sdk::unstable::build_prompt_builder;

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
    /// The background jobs, when offered. `/reset` stops them all; dropping
    /// the last handle at exit kills any still running.
    pub background: Option<Arc<BackgroundSupervisor>>,
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
    /// The switch `/plan-mode` flips; the outermost approval gate reads it.
    pub plan_mode: PlanMode,
    /// The whole approval stack the loop asks, outermost first:
    /// `PlanModeGate(HookGate(RuleGate(CliApprovalGate)))`, the hook layer
    /// only when hooks are configured (see [`agent_sdk::unstable`]).
    pub approval: Arc<dyn ApprovalGate>,
    /// The operator's hooks (`AGENT_HOOKS`), when configured. The gate and
    /// the post-hook wrapping hold them too; a turn is begun and ended
    /// through this handle, because only the subcommand knows the session
    /// and when a turn is over.
    pub hooks: Option<Arc<Hooks>>,
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

/// Wires the whole agent from resolved settings, with the terminal's
/// approval prompt and notifications as the gate that asks a human.
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

    let (assembly, (notifications, remote_approval)) = unstable::assemble(
        settings,
        unstable::Wiring {
            events,
            model,
            environment,
            retention: retention.map(|resume| match resume {
                Resume::No => Spare::Nothing,
                Resume::Latest => Spare::Latest,
                Resume::Session(id) => Spare::Session(id),
            }),
            asks: Box::new(move |context| Box::pin(terminal_gate(context, interactive))),
        },
    )
    .await?;

    let Assembly {
        settings,
        workspace,
        tools,
        provider,
        agent,
        sessions,
        commands,
        background,
        tool_servers,
        redactor,
        checkpoints,
        plan_mode,
        approval,
        hooks,
        files,
    } = assembly;
    Ok(Application {
        settings,
        workspace,
        tools,
        provider,
        agent,
        sessions,
        commands,
        background,
        notifications,
        remote_approval,
        tool_servers,
        redactor,
        checkpoints,
        plan_mode,
        approval,
        hooks,
        files,
    })
}

/// The gate that asks the person at the terminal (or, with remote approval,
/// their phone), and the notifications it shares with the subcommands.
///
/// The notifications go through the same redactor as tool output: a secret
/// hidden from the model must not leave by the other door.
async fn terminal_gate(
    context: GateContext,
    interactive: bool,
) -> Result<(
    Arc<dyn ApprovalGate>,
    (Arc<Notifications>, Arc<dyn RemoteApprover>),
)> {
    let GateContext { settings, redactor } = context;
    // No URL means `NullNotifier`, not a flag: nothing above this line has a
    // "notifications are off" branch, and nothing is ever sent.
    let notifier: Arc<dyn Notifier> = match webhook(&settings.notify)? {
        Some(webhook) => Arc::new(webhook),
        None => Arc::new(NullNotifier),
    };
    let notifications = Arc::new(
        Notifications::new(notifier, NotifyPolicy::from_settings(&settings.notify))
            .with_redactor(redactor),
    );

    let remote_approval = remote_approver(&settings.notify, interactive).await?;
    let asks: Arc<dyn ApprovalGate> = Arc::new(
        CliApprovalGate::new(settings.approval, interactive, notifications.clone())
            .with_dependency_changes_allowed(settings.allow_dependency_changes)
            .with_remote(remote_approval.clone()),
    );
    Ok((asks, (notifications, remote_approval)))
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
