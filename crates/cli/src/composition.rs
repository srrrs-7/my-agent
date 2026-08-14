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
use agent_application::tools::ToolRegistry;
use agent_application::tools::exec::RunCommandTool;
use agent_application::tools::file::{
    EditFileTool, ListDirectoryTool, ReadFileTool, SearchFilesTool, WriteFileTool,
};
use agent_application::tools::web::WebFetchTool;
use agent_domain::model::llm::{GenerationParams, ModelId};
use agent_domain::model::workspace::WorkspaceRoot;
use agent_domain::ports::approval::ApprovalGate;
use agent_domain::ports::events::EventSink;
use agent_domain::ports::llm::LlmProvider;
use agent_domain::ports::prompt::PromptBuilder;
use agent_domain::ports::tool::Tool;
use agent_infrastructure::config::{ApprovalPolicy, PromptSettings, Settings, ShellSettings};
use agent_infrastructure::exec::{ExecConfig, SandboxedCommandRunner};
use agent_infrastructure::fs::{IgnoreAwareSearcher, LocalFileSystem, WorkspaceContextProvider};
use agent_infrastructure::llm::build_provider;
use agent_infrastructure::tools::TimeoutTool;
use agent_infrastructure::web::{GuardedWebFetcher, WebFetchConfig};
use anyhow::{Context, Result};

use crate::approval::CliApprovalGate;
use crate::args::Cli;
use crate::render::TerminalRenderer;

pub struct Application {
    pub settings: Settings,
    pub workspace: Arc<WorkspaceRoot>,
    pub tools: Arc<ToolRegistry>,
    pub provider: Arc<dyn LlmProvider>,
    pub agent: AgentLoop,
    /// Kept alive for the life of the process: it owns the egress proxy's
    /// port, and the commands it confines refer to that port by number.
    /// `doctor` also reads its reported sandbox rather than the configuration.
    pub commands: Option<Arc<SandboxedCommandRunner>>,
}

/// Async because the egress proxy binds a socket at startup - see
/// [`SandboxedCommandRunner::start`].
pub async fn build(cli: &Cli, interactive: bool) -> Result<Application> {
    let settings = resolve_settings(cli)?;

    // --- workspace sandbox ---------------------------------------------------
    let canonical = std::fs::canonicalize(&settings.workspace).with_context(|| {
        format!(
            "workspace `{}` does not exist or is not readable",
            settings.workspace.display()
        )
    })?;
    let workspace =
        Arc::new(WorkspaceRoot::new(canonical).context("the workspace path must be absolute")?);

    // --- adapters ------------------------------------------------------------
    let file_system = Arc::new(
        LocalFileSystem::new(workspace.clone(), settings.max_file_bytes)
            .context("cannot open the workspace")?,
    );
    let searcher = Arc::new(IgnoreAwareSearcher::new(workspace.clone()));
    let context = Arc::new(WorkspaceContextProvider::new(workspace.clone()));
    let provider = build_provider(&settings.llm).context("cannot build the LLM provider")?;

    let approval: Arc<dyn ApprovalGate> =
        Arc::new(CliApprovalGate::new(settings.approval, interactive));
    let events: Arc<dyn EventSink> = Arc::new(TerminalRenderer::new(cli.verbose, cli.no_color));

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

        let mut registry = (*tools).clone();
        registry.register(TimeoutTool::wrap(
            Arc::new(WebFetchTool::new(Arc::new(fetcher))),
            settings.agent_loop.tool_timeout,
        ));
        tools = Arc::new(registry);
    }

    // run_command is opt-in for a stronger reason than web_fetch: a command's
    // effect cannot be read off its arguments. Startup fails outright when the
    // sandbox is weaker than asked for, rather than registering the tool with
    // less protection than the operator believes it has.
    let commands = if settings.shell.enabled {
        let runner = build_command_runner(&settings.shell, workspace.clone()).await?;

        let mut registry = (*tools).clone();
        registry.register(TimeoutTool::wrap(
            Arc::new(RunCommandTool::new(
                runner.clone(),
                &settings.shell.allowed_domains,
            )),
            // The command has its own timeout; this outer one only catches a
            // runner that never returns at all.
            settings.agent_loop.tool_timeout + std::time::Duration::from_secs(600),
        ));
        tools = Arc::new(registry);
        Some(runner)
    } else {
        None
    };

    // --- the loop ------------------------------------------------------------
    let prompt = build_prompt_builder(&settings.prompt)?;
    let agent = AgentLoop::new(
        AgentDependencies {
            llm: provider.clone(),
            tools: tools.clone(),
            approval,
            events,
            context,
            prompt,
        },
        AgentLoopConfig {
            model: cli.model.as_deref().map(ModelId::new),
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
            parallel_read_only_tools: settings.agent_loop.parallel_read_only_tools,
            stream: settings.agent_loop.stream,
        },
    );

    Ok(Application {
        settings,
        workspace,
        tools,
        provider,
        agent,
        commands,
    })
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
) -> Result<Arc<SandboxedCommandRunner>> {
    let runner = SandboxedCommandRunner::start(
        workspace,
        ExecConfig {
            sandbox: shell.sandbox,
            extra_writable: shell.extra_writable.clone(),
            allowed_domains: shell.allowed_domains.clone(),
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

/// The default toolset, every tool wrapped in a timeout so that one
/// pathological call cannot stall the loop.
fn build_tools(
    file_system: Arc<LocalFileSystem>,
    searcher: Arc<IgnoreAwareSearcher>,
    workspace: Arc<WorkspaceRoot>,
    timeout: std::time::Duration,
) -> Arc<ToolRegistry> {
    let timed = |tool: Arc<dyn Tool>| TimeoutTool::wrap(tool, timeout);
    Arc::new(
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
            .with(timed(Arc::new(SearchFilesTool::new(searcher, workspace)))),
    )
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
fn build_prompt_builder(prompt: &PromptSettings) -> Result<Arc<dyn PromptBuilder>> {
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
fn resolve_settings(cli: &Cli) -> Result<Settings> {
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

    fn snapshot() -> ContextSnapshot {
        ContextSnapshot {
            workspace_root: "/workspace".into(),
            os: "linux".into(),
            today: "2026-08-14".into(),
            is_git_repository: true,
            project_instructions: Some("Project rules here.".into()),
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
        })
        .unwrap();

        let prompt = builder.build(&snapshot(), &[]);
        assert_eq!(prompt, "You are a terse bot.\n\nReply in Japanese.\n");
        assert!(
            !prompt.contains("Project rules here."),
            "a replaced prompt carries none of the default sections"
        );
    }

    #[test]
    fn cli_flags_override_the_environment() {
        use agent_infrastructure::config::MapEnv;

        let env = MapEnv::new(&[
            ("AGENT_WORKSPACE", "/workspace"),
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
