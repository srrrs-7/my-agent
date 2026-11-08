//! `agent eval <tasks.json>` - run a task suite against the configured model
//! and report how it went, as JSON on stdout.
//!
//! Each attempt gets a scratch copy of its fixture and an agent assembled for
//! it by [`composition::assemble`] - the same wiring `agent run` uses, so
//! what is measured is what ships. Nothing here touches the real workspace.
//!
//! The order of operations is the contract with the operator's bill: every
//! way the run can fail on configuration - a bad suite, a missing fixture, a
//! sandbox the command checks need but cannot have - fails before the first
//! request reaches the model.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Instant;

use agent_application::agent::Session;
use agent_application::eval::{
    AttemptInput, AttemptReport, Fingerprint, MetricsSink, SuiteReport, Task, TaskReport,
    TaskSuite, verify,
};
use agent_application::eval::{CheckOutcome, PromptSource};
use agent_domain::model::context::ContextSnapshot;
use agent_domain::model::llm::ModelId;
use agent_domain::ports::command::CommandRunner;
use agent_domain::ports::events::{EventSink, NullEventSink};
use agent_infrastructure::config::{ApprovalPolicy, Settings, SystemEnv};
use agent_infrastructure::exec::{ExecConfig, SandboxRequirement, SandboxedCommandRunner};
use agent_infrastructure::fs::{LocalFileSystem, ScratchDir};
use anyhow::{Context, Result};

use crate::args::{Cli, EvalArgs};
use crate::composition::{self, Application, Wiring};
use crate::session_id;

/// Some attempt did not pass. Distinct from 1, which means the suite could
/// not be run at all, so a script can tell "worse" from "broken".
pub const EXIT_TASKS_FAILED: u8 = 3;

/// Names every scratch workspace, so a kept or leaked one is recognisable.
const SCRATCH_PREFIX: &str = "agent-eval-";

/// A task with its operator paths resolved and its prompt read.
struct PreparedTask {
    task: Task,
    fixture: PathBuf,
    prompt: String,
}

/// What every attempt shares.
struct Plan {
    settings: Settings,
    model: Option<ModelId>,
    environment: Vec<(String, String)>,
    /// How command checks are confined. `None` when no task has one.
    checks: Option<ExecConfig>,
    keep: bool,
}

pub async fn execute(cli: &Cli, args: &EvalArgs) -> Result<ExitCode> {
    let tasks = load_suite(&args.tasks)?;

    let mut settings = composition::resolve_settings(cli)?;
    adapt_settings(&mut settings, &args.out)?;
    let has_command_checks = tasks
        .iter()
        .any(|prepared| prepared.task.has_command_checks());
    let plan = Plan {
        checks: has_command_checks.then(|| check_config(&settings)),
        settings,
        model: cli.model.as_deref().map(ModelId::new),
        environment: SystemEnv.vars(),
        keep: args.keep,
    };
    let fingerprint = preflight(&plan).await?;

    eprintln!(
        "eval: {} task(s) x {} on {}/{}",
        tasks.len(),
        args.repeat,
        fingerprint.provider,
        fingerprint.model
    );
    let total = tasks.len() * args.repeat as usize;
    let mut done = 0;
    let mut attempts: Vec<Vec<AttemptReport>> = vec![Vec::new(); tasks.len()];
    // Repeats on the outside, so one task's attempts are spread over the run
    // rather than bunched: a provider that degrades halfway through shows up
    // across tasks instead of as one task's bad luck.
    for attempt in 1..=args.repeat {
        for (index, task) in tasks.iter().enumerate() {
            done += 1;
            let report = run_attempt(&plan, task, attempt).await?;
            print_progress(done, total, task, &report);
            attempts[index].push(report);
        }
    }

    let report = SuiteReport::new(
        fingerprint,
        args.repeat,
        tasks
            .into_iter()
            .zip(attempts)
            .map(|(task, attempts)| TaskReport::new(task.task.id, attempts))
            .collect(),
    );
    eprintln!(
        "eval: {}/{} passed ({:.0}%)",
        report.summary.totals.passed,
        report.summary.totals.attempts,
        report.summary.totals.pass_rate * 100.0
    );
    println!(
        "{}",
        serde_json::to_string_pretty(&report).context("cannot serialise the report")?
    );

    Ok(if report.all_passed() {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(EXIT_TASKS_FAILED)
    })
}

/// Reads and validates the suite, resolving its paths against the suite
/// file's directory and reading every prompt file up front - a missing one
/// is found now, not after half the suite has been paid for.
fn load_suite(path: &Path) -> Result<Vec<PreparedTask>> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("cannot read the task file `{}`", path.display()))?;
    let suite = TaskSuite::parse(&text)
        .with_context(|| format!("invalid task file `{}`", path.display()))?;
    let base = path.parent().unwrap_or(Path::new(""));

    suite
        .tasks
        .into_iter()
        .map(|task| {
            let fixture = base.join(&task.fixture);
            anyhow::ensure!(
                fixture.is_dir(),
                "task `{}`: the fixture `{}` is not a directory (paths are relative to the task \
                 file)",
                task.id,
                fixture.display()
            );
            let prompt = match &task.prompt {
                PromptSource::Inline(text) => text.clone(),
                PromptSource::File(file) => {
                    let file = base.join(file);
                    std::fs::read_to_string(&file).with_context(|| {
                        format!(
                            "task `{}`: cannot read the prompt file `{}`",
                            task.id,
                            file.display()
                        )
                    })?
                }
            };
            anyhow::ensure!(
                !prompt.trim().is_empty(),
                "task `{}`: the prompt is empty",
                task.id
            );
            Ok(PreparedTask {
                task,
                fixture,
                prompt,
            })
        })
        .collect()
}

/// Turns the operator's settings into the ones every attempt starts from.
fn adapt_settings(settings: &mut Settings, out: &Path) -> Result<()> {
    // Nobody is there to answer, and what the model changes is a copy that
    // is thrown away. Dependency changes are still asked about - and so
    // refused, since no one answers - unless the operator allowed them.
    settings.approval = ApprovalPolicy::Auto;
    // A suite run is not a turn anyone is waiting on.
    settings.notify.webhook = None;

    // Absolute, because a relative session dir is read against the
    // workspace - which is a different scratch directory every attempt.
    let out = std::path::absolute(out)
        .with_context(|| format!("cannot resolve the output directory `{}`", out.display()))?;
    settings.session.dir = out.join("sessions");
    std::fs::create_dir_all(&settings.session.dir).with_context(|| {
        format!(
            "cannot create the session directory `{}`",
            settings.session.dir.display()
        )
    })?;

    // `run_command` without confinement would let the model's commands reach
    // the operator's real files from inside a run meant to touch nothing.
    // An explicit AGENT_SHELL_SANDBOX=none is the only way to get there; in
    // eval it removes the tool instead.
    if settings.shell.enabled && settings.shell.sandbox == SandboxRequirement::Disabled {
        eprintln!(
            "eval: run_command is not offered - AGENT_SHELL_SANDBOX=none would run the model's \
             commands unconfined"
        );
        settings.shell.enabled = false;
    }
    Ok(())
}

/// How command checks are confined.
///
/// A check like `cargo test` executes code the model wrote (`build.rs`, the
/// tests themselves), so it gets at least the confinement `run_command`
/// would. `none` is raised to `confined` rather than honoured: it is a
/// statement about the model's own commands, made by an operator who trusts
/// the container, and a check is not an occasion to extend it.
fn check_config(settings: &Settings) -> ExecConfig {
    let shell = &settings.shell;
    ExecConfig {
        sandbox: match shell.sandbox {
            SandboxRequirement::Disabled => SandboxRequirement::Confined,
            other => other,
        },
        extra_writable: shell.extra_writable.clone(),
        allowed_domains: shell.allowed_domains.clone(),
        withheld: settings.configured_secrets(),
    }
}

/// Assembles the agent once over an empty scratch directory - which proves
/// the configuration without a model request - and starts the checks'
/// sandbox there, so a platform that cannot confine them fails the run now.
async fn preflight(plan: &Plan) -> Result<Fingerprint> {
    let scratch = ScratchDir::empty(SCRATCH_PREFIX).context("cannot create a scratch workspace")?;
    let app = assemble(
        plan,
        plan.settings.clone(),
        scratch.path(),
        Arc::new(NullEventSink),
    )
    .await?;

    let check_sandbox = match &plan.checks {
        Some(config) => {
            let runner = SandboxedCommandRunner::start(app.workspace.clone(), config.clone())
                .await
                .context(
                    "the suite has command checks, and they run under the command sandbox, \
                     which this platform cannot provide (Linux 6.7+ with Landlock is needed). \
                     Run the eval in the dev container, or remove the command checks",
                )?;
            Some(runner.sandbox().as_str().to_string())
        }
        None => None,
    };

    fingerprint(plan, &app, check_sandbox)
}

/// [`composition::assemble`] over `workspace`, non-interactive, narrating to
/// `events`.
async fn assemble(
    plan: &Plan,
    mut settings: Settings,
    workspace: &Path,
    events: Arc<dyn EventSink>,
) -> Result<Application> {
    settings.workspace = workspace.to_path_buf();
    let app = composition::assemble(
        settings,
        Wiring {
            interactive: false,
            events,
            model: plan.model.clone(),
            environment: plan.environment.clone(),
            retention: None,
        },
    )
    .await?;

    // Unreachable today - `adapt_settings` turns the tool off, and the runner
    // refuses to start below its requirement - but this is the property the
    // run depends on, so it is checked where it holds rather than assumed.
    if let Some(runner) = &app.commands {
        anyhow::ensure!(
            runner.sandbox().confines(),
            "run_command came up without confinement; refusing to evaluate with it"
        );
    }
    Ok(app)
}

fn fingerprint(
    plan: &Plan,
    app: &Application,
    check_sandbox: Option<String>,
) -> Result<Fingerprint> {
    let settings = &plan.settings;
    let model = plan
        .model
        .clone()
        .unwrap_or_else(|| settings.default_provider_settings().model.clone());
    let tools = if app.provider.capabilities().supports_tools {
        app.tools.definitions()
    } else {
        Vec::new()
    };

    // Rendered against a fixed context, so the hash moves when the prompt
    // template, the operator's replacement or appendix, or the advertised
    // tools do - and not with the scratch path, the date or the fixture,
    // which change on every run without changing what is being measured.
    let placeholder = ContextSnapshot {
        workspace_root: "<workspace>".into(),
        os: "<os>".into(),
        today: "<today>".into(),
        is_git_repository: false,
        project_instructions: None,
        nested_instructions: Vec::new(),
        directory_overview: Vec::new(),
    };
    let prompt = composition::build_prompt_builder(&settings.prompt)?.build(&placeholder, &tools);

    let loop_settings = &settings.agent_loop;
    Ok(Fingerprint {
        agent_version: env!("CARGO_PKG_VERSION").to_string(),
        provider: app.provider.id().to_string(),
        model: model.to_string(),
        tools: tools.iter().map(|tool| tool.name.to_string()).collect(),
        run_command_sandbox: app
            .commands
            .as_ref()
            .map(|runner| runner.sandbox().as_str().to_string()),
        check_sandbox,
        settings: serde_json::json!({
            "max_iterations": loop_settings.max_iterations,
            "max_tokens": loop_settings.max_tokens,
            "temperature": loop_settings.temperature,
            "max_history_bytes": loop_settings.max_history_bytes,
            "max_tool_output_bytes": loop_settings.max_tool_output_bytes,
            "compact": loop_settings.compact,
            "compact_keep_recent": loop_settings.compact_keep_recent,
            "compact_keep_recent_bytes": loop_settings.compact_keep_recent_bytes,
            "parallel_read_only_tools": loop_settings.parallel_read_only_tools,
            "stream": loop_settings.stream,
            "router": format!("{:?}", settings.llm.router),
            "prompt_cache": format!("{:?}", settings.llm.prompt_cache),
            "thinking": format!("{:?}", settings.llm.thinking),
            "effort": format!("{:?}", settings.llm.effort),
            "allow_dependency_changes": settings.allow_dependency_changes,
            "web_fetch": settings.web_fetch.enabled,
            "run_command": settings.shell.enabled,
        }),
        system_prompt_fnv1a: Fingerprint::prompt_hash(&prompt),
    })
}

/// One attempt at one task, in a scratch copy of its fixture.
///
/// A provider failure is recorded in the attempt, not returned: the rest of
/// the suite can still run, and the report has to say which task it hit.
/// What is returned as an error is a failure to set the attempt up at all.
async fn run_attempt(plan: &Plan, task: &PreparedTask, attempt: u32) -> Result<AttemptReport> {
    // Declared first so it is dropped last: the agent below still has the
    // directory open until it goes.
    let mut scratch = ScratchDir::copy_of(&task.fixture, SCRATCH_PREFIX)
        .with_context(|| format!("task `{}`: cannot copy the fixture", task.task.id))?;
    if plan.keep {
        scratch = scratch.keep();
    }

    let metrics = Arc::new(MetricsSink::new());
    let mut settings = plan.settings.clone();
    if let Some(limit) = task.task.max_iterations {
        settings.agent_loop.max_iterations = limit;
    }
    let app = assemble(plan, settings, scratch.path(), metrics.clone())
        .await
        .with_context(|| format!("task `{}`: cannot assemble the agent", task.task.id))?;

    // Started before the model is asked anything, so a sandbox that went
    // away since the preflight costs no tokens.
    let checks_runner = match &plan.checks {
        Some(config) if task.task.has_command_checks() => Some(
            SandboxedCommandRunner::start(app.workspace.clone(), config.clone())
                .await
                .with_context(|| {
                    format!("task `{}`: cannot start the check sandbox", task.task.id)
                })?,
        ),
        _ => None,
    };

    let session_id = format!("{}-{attempt}-{}", task.task.id, session_id());
    let mut session = Session::new(session_id.clone());
    let started = Instant::now();
    let result = app.agent.run(&mut session, task.prompt.clone()).await;
    let wall = started.elapsed();

    let files = LocalFileSystem::new(app.workspace.clone(), app.settings.max_file_bytes)
        .context("cannot open the scratch workspace to check it")?;
    let checks = verify(
        &task.task.checks,
        &files,
        checks_runner
            .as_ref()
            .map(|runner| runner as &dyn CommandRunner),
    )
    .await;

    Ok(AttemptReport::new(AttemptInput {
        attempt,
        finish: result
            .map(|outcome| outcome.reason)
            .map_err(|error| error.to_string()),
        usage: session.usage,
        metrics: metrics.snapshot(),
        wall,
        session_id,
        scratch_dir: scratch
            .is_kept()
            .then(|| scratch.path().display().to_string()),
        checks,
    }))
}

fn print_progress(done: usize, total: usize, task: &PreparedTask, report: &AttemptReport) {
    eprintln!(
        "[{done}/{total}] {} #{} {} ({}, {} iteration(s), {} tokens, {:.1}s)",
        task.task.id,
        report.attempt,
        if report.passed { "PASS" } else { "FAIL" },
        report.finish_reason,
        report.iterations,
        report.tokens.total,
        report.wall_ms as f64 / 1000.0
    );
    if let Some(error) = &report.error {
        eprintln!("      error: {error}");
    }
    for check in report.checks.iter().filter(|check| !check.passed) {
        print_failed_check(check);
    }
    if let Some(dir) = &report.scratch_dir {
        eprintln!("      kept: {dir}");
    }
}

fn print_failed_check(check: &CheckOutcome) {
    eprintln!("      {} {}: {}", check.kind, check.target, check.detail);
}
