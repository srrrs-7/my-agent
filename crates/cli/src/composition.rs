//! The composition root.
//!
//! This is the only place in the program where concrete types meet the ports
//! they implement. Every layer above receives `Arc<dyn Trait>` and therefore
//! stays swappable - which is what makes "point the agent at Ollama instead of
//! Anthropic", "auto-approve in CI", or "add a router" configuration changes
//! rather than code changes.

use std::sync::Arc;

use agent_application::agent::{
    AgentDependencies, AgentLoop, AgentLoopConfig, DefaultPromptBuilder,
};
use agent_application::tools::ToolRegistry;
use agent_application::tools::file::{
    EditFileTool, ListDirectoryTool, ReadFileTool, SearchFilesTool, WriteFileTool,
};
use agent_domain::model::llm::{GenerationParams, ModelId};
use agent_domain::model::workspace::WorkspaceRoot;
use agent_domain::ports::approval::ApprovalGate;
use agent_domain::ports::events::EventSink;
use agent_domain::ports::llm::LlmProvider;
use agent_domain::ports::tool::Tool;
use agent_infrastructure::config::{ApprovalPolicy, Settings};
use agent_infrastructure::fs::{IgnoreAwareSearcher, LocalFileSystem, WorkspaceContextProvider};
use agent_infrastructure::llm::build_provider;
use agent_infrastructure::tools::TimeoutTool;
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
}

pub fn build(cli: &Cli, interactive: bool) -> Result<Application> {
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

    let tools = build_tools(
        file_system,
        searcher,
        workspace.clone(),
        settings.agent_loop.tool_timeout,
    );

    // --- the loop ------------------------------------------------------------
    let agent = AgentLoop::new(
        AgentDependencies {
            llm: provider.clone(),
            tools: tools.clone(),
            approval,
            events,
            context,
            prompt: Arc::new(DefaultPromptBuilder),
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
    })
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

/// Environment first, command-line flags on top.
fn resolve_settings(cli: &Cli) -> Result<Settings> {
    let mut settings = Settings::from_env()
        .context("invalid configuration - copy .env.example to .env (make env) and fill it in")?;

    if let Some(workspace) = &cli.workspace {
        settings.workspace = workspace.clone();
    }
    if let Some(max_iterations) = cli.max_iterations {
        settings.agent_loop.max_iterations = max_iterations.max(1);
    }
    if cli.yes {
        settings.approval = ApprovalPolicy::Auto;
    }

    Ok(settings)
}
