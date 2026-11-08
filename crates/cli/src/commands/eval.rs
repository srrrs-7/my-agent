//! `agent eval <tasks.json>` - run a task suite against the configured model
//! and report how it went, as JSON on stdout.
//!
//! Each attempt gets a scratch copy of its fixture and an agent assembled for
//! it by [`composition::assemble`] - the same wiring `agent run` uses, so
//! what is measured is what ships. Nothing here touches the real workspace.
//!
//! The order of operations is the contract with the operator's bill: every
//! way the run can fail on configuration - a bad suite, a missing fixture or
//! one holding a link, a sandbox the command checks need but cannot have -
//! fails before the first request reaches the model. Once requests are being
//! paid for, nothing ends the run without a report: an attempt that cannot be
//! set up is recorded as such and the suite goes on, and Ctrl-C stops it with
//! the report of what ran so far.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::{Duration, Instant};

use agent_application::agent::{Interrupt, Session};
use agent_application::eval::{
    AttemptInput, AttemptReport, Fingerprint, MetricsSink, SuiteReport, Task, TaskReport,
    TaskSuite, fnv1a64, verify,
};
use agent_application::eval::{CheckOutcome, PromptSource};
use agent_domain::model::context::ContextSnapshot;
use agent_domain::model::llm::ModelId;
use agent_domain::ports::command::CommandRunner;
use agent_domain::ports::events::{EventSink, FinishReason, NullEventSink};
use agent_domain::text::printable;
use agent_infrastructure::config::{ApprovalPolicy, Settings, SystemEnv};
use agent_infrastructure::exec::{ExecConfig, SandboxRequirement, SandboxedCommandRunner};
use agent_infrastructure::fs::{LocalFileSystem, ScratchDir};
use agent_infrastructure::tools::Redactor;
use anyhow::{Context, Result};

use crate::args::{Cli, EvalArgs};
use crate::composition::{self, Application, Wiring};
use crate::ctrl_c::{CtrlC, EXIT_INTERRUPTED, Sigint, TurnEnd, drive};
use crate::session_id;

/// Every attempt ran, and some did not pass. Distinct from 1 - the suite, or
/// some attempt in it, could not be run (a bad suite, a broken provider, an
/// attempt that could not be set up) - so a script can tell "worse" from
/// "broken": a pass rate is only worth comparing when this is the code.
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

/// The attempts made, per task, and whether Ctrl-C cut the run short.
struct SuiteRun {
    attempts: Vec<Vec<AttemptReport>>,
    interrupted: bool,
}

pub async fn execute(cli: &Cli, args: &EvalArgs) -> Result<ExitCode> {
    let tasks = load_suite(&args.tasks)?;

    let mut settings = composition::resolve_settings(cli)?;
    adapt_settings(&mut settings, args)?;
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
    let (fingerprint, redactor) = preflight(&plan, args).await?;

    eprintln!(
        "eval: {} task(s) x {} on {}/{}",
        tasks.len(),
        args.repeat,
        fingerprint.provider,
        fingerprint.model
    );
    // Installed only now, so a Ctrl-C during the setup above still ends the
    // process the usual way: there is no report to save yet.
    let mut sigint = Sigint::listen()?;
    let run = run_suite(&plan, &tasks, args.repeat, &redactor, &mut sigint).await;

    let mut report = SuiteReport::new(
        fingerprint,
        args.repeat,
        tasks
            .into_iter()
            .zip(run.attempts)
            .map(|(task, attempts)| TaskReport::new(task.task.id, attempts))
            .collect(),
    );
    report.interrupted = run.interrupted;
    eprintln!(
        "eval: {}/{} passed ({:.0}%){}",
        report.summary.totals.passed,
        report.summary.totals.attempts,
        report.summary.totals.pass_rate * 100.0,
        if report.interrupted {
            " - interrupted, the report covers the attempts made so far"
        } else {
            ""
        }
    );
    println!(
        "{}",
        serde_json::to_string_pretty(&report).context("cannot serialise the report")?
    );

    Ok(exit_code(&report))
}

/// Interrupted first - the operator knows why the numbers are partial - then
/// "could not run", which makes the pass rate meaningless, then "did not
/// pass".
fn exit_code(report: &SuiteReport) -> ExitCode {
    if report.interrupted {
        ExitCode::from(EXIT_INTERRUPTED)
    } else if !report.all_ran() {
        ExitCode::FAILURE
    } else if !report.all_passed() {
        ExitCode::from(EXIT_TASKS_FAILED)
    } else {
        ExitCode::SUCCESS
    }
}

/// Reads and validates the suite, resolving its paths against the suite
/// file's directory, reading every prompt file and checking every fixture up
/// front - a missing prompt or a fixture with a link in it is found now, not
/// after half the suite has been paid for.
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
            ScratchDir::check_fixture(&fixture)
                .with_context(|| format!("task `{}`: the fixture cannot be used", task.id))?;
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
fn adapt_settings(settings: &mut Settings, args: &EvalArgs) -> Result<()> {
    // Nobody is there to answer. Approving everything is safe only because
    // of what is left below: the file tools touch a copy that is thrown
    // away, and run_command is confined. Dependency changes are still asked
    // about - and so refused, since no one answers - unless the operator
    // allowed them.
    settings.approval = ApprovalPolicy::Auto;
    // A suite run is not a turn anyone is waiting on.
    settings.notify.webhook = None;

    // The tools whose effects leave the machine are where "it is only a
    // scratch copy" stops being true: an MCP server acts on whatever it is
    // connected to, and a fetched URL is a message sent. Auto-approving them
    // would be the operator's environment deciding, not the operator - so a
    // suite gets them only when the command line asks.
    if !args.allow_mcp && settings.mcp.config.take().is_some() {
        eprintln!(
            "eval: the MCP servers in AGENT_MCP_CONFIG are not started - pass --allow-mcp to \
             evaluate with them"
        );
    }
    if !args.allow_web && settings.web_fetch.enabled {
        eprintln!("eval: web_fetch is not offered - pass --allow-web to evaluate with it");
        settings.web_fetch.enabled = false;
    }
    // A query is a message sent, like a URL.
    if !args.allow_web && settings.web_search.enabled {
        eprintln!("eval: web_search is not offered - pass --allow-web to evaluate with it");
        settings.web_search.enabled = false;
    }

    // Absolute, because a relative session dir is read against the
    // workspace - which is a different scratch directory every attempt.
    let out = &args.out;
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
///
/// Also hands back the redactor that assembly built, for what the report
/// quotes from the attempts.
async fn preflight(plan: &Plan, args: &EvalArgs) -> Result<(Fingerprint, Arc<Redactor>)> {
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

    Ok((
        fingerprint(plan, args, &app, check_sandbox)?,
        app.redactor.clone(),
    ))
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
    args: &EvalArgs,
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

    // The placeholder context leaves the global instructions out, yet every
    // attempt's prompt carries them - so their contents are hashed apart.
    let global_instructions = settings
        .prompt
        .global_instructions
        .as_ref()
        .map(|path| {
            std::fs::read(path)
                .map(|contents| format!("{:016x}", fnv1a64(&contents)))
                .with_context(|| {
                    format!(
                        "cannot read AGENT_GLOBAL_INSTRUCTIONS `{}` to fingerprint it",
                        path.display()
                    )
                })
        })
        .transpose()?;

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
            "tool_timeout_secs": loop_settings.tool_timeout.as_secs(),
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
            "allow_mcp": args.allow_mcp,
            "allow_web": args.allow_web,
            "mcp_servers": app.tool_servers.len(),
            "web_fetch": settings.web_fetch.enabled,
            "web_search": settings.web_search.enabled,
            "run_command": settings.shell.enabled,
            "global_instructions_fnv1a": global_instructions,
        }),
        system_prompt_fnv1a: Fingerprint::prompt_hash(&prompt),
    })
}

/// Every attempt of every task, with Ctrl-C routed the way `agent run` routes
/// it: the first press interrupts the running attempt and ends the suite
/// after it, the second abandons the attempt at once. Either way the scratch
/// copy goes with the attempt, and the attempts already made are returned.
async fn run_suite(
    plan: &Plan,
    tasks: &[PreparedTask],
    repeat: u32,
    redactor: &Redactor,
    sigint: &mut Sigint,
) -> SuiteRun {
    let mut keys = CtrlC::new();
    let total = tasks.len() * repeat as usize;
    let mut done = 0;
    let mut attempts: Vec<Vec<AttemptReport>> = vec![Vec::new(); tasks.len()];
    // Repeats on the outside, so one task's attempts are spread over the run
    // rather than bunched: a provider that degrades halfway through shows up
    // across tasks instead of as one task's bad luck.
    for attempt in 1..=repeat {
        for (index, task) in tasks.iter().enumerate() {
            done += 1;
            let interrupt = Interrupt::new();
            let running = run_attempt(plan, task, attempt, redactor, &interrupt);
            match drive(running, &interrupt, sigint, &mut keys).await {
                TurnEnd::Finished(report) => {
                    print_progress(done, total, task, &report);
                    attempts[index].push(report);
                    if interrupt.is_fired() {
                        return SuiteRun {
                            attempts,
                            interrupted: true,
                        };
                    }
                }
                TurnEnd::Abandoned(_) => {
                    return SuiteRun {
                        attempts,
                        interrupted: true,
                    };
                }
            }
        }
    }
    SuiteRun {
        attempts,
        interrupted: false,
    }
}

/// One attempt at one task, in a scratch copy of its fixture.
///
/// Never fails: an attempt that could not be set up - the fixture would not
/// copy, the agent would not assemble, the check sandbox would not start -
/// is reported as an attempt that did not run, with why, and the rest of the
/// suite goes on. So is a provider failure, which still has its checks run.
async fn run_attempt(
    plan: &Plan,
    task: &PreparedTask,
    attempt: u32,
    redactor: &Redactor,
    interrupt: &Interrupt,
) -> AttemptReport {
    let session_id = format!("{}-{attempt}-{}", task.task.id, session_id());
    let metrics = Arc::new(MetricsSink::new());
    let mut session = Session::new(session_id.clone());

    // Owned out here, and so dropped last: the agent inside still has the
    // directory open until it goes.
    let scratch = ScratchDir::copy_of(&task.fixture, SCRATCH_PREFIX)
        .map(|scratch| if plan.keep { scratch.keep() } else { scratch })
        .context("cannot copy the fixture");
    let scratch_dir = scratch
        .as_ref()
        .ok()
        .filter(|scratch| scratch.is_kept())
        .map(|scratch| scratch.path().display().to_string());

    let ran = match &scratch {
        Ok(scratch) => {
            attempt_in(
                plan,
                task,
                scratch.path(),
                metrics.clone(),
                &mut session,
                interrupt,
            )
            .await
        }
        Err(error) => Err(anyhow::anyhow!("{error:#}")),
    };
    let Ran {
        finish,
        wall,
        checks,
    } = ran.unwrap_or_else(|error| Ran {
        finish: Err(format!("the attempt could not be set up: {error:#}")),
        wall: Duration::ZERO,
        checks: Vec::new(),
    });

    AttemptReport::new(AttemptInput {
        attempt,
        finish: finish.map_err(|error| redactor.redact(&error).into_owned()),
        usage: session.usage,
        metrics: metrics.snapshot(),
        wall,
        session_id,
        scratch_dir,
        checks: redact_checks(checks, redactor),
    })
}

/// What a set-up attempt amounted to.
struct Ran {
    finish: Result<FinishReason, String>,
    /// The model's part of the attempt, setup and checks excluded.
    wall: Duration,
    checks: Vec<CheckOutcome>,
}

/// The attempt proper, over a workspace that already holds the fixture.
/// `Err` is a failure to set it up, before the model was asked anything.
async fn attempt_in(
    plan: &Plan,
    task: &PreparedTask,
    workspace: &Path,
    metrics: Arc<MetricsSink>,
    session: &mut Session,
    interrupt: &Interrupt,
) -> Result<Ran> {
    let mut settings = plan.settings.clone();
    if let Some(limit) = task.task.max_iterations {
        settings.agent_loop.max_iterations = limit;
    }
    let app = assemble(plan, settings, workspace, metrics)
        .await
        .context("cannot assemble the agent")?;

    // Started before the model is asked anything, so a sandbox that went
    // away since the preflight costs no tokens.
    let checks_runner = match &plan.checks {
        Some(config) if task.task.has_command_checks() => Some(
            SandboxedCommandRunner::start(app.workspace.clone(), config.clone())
                .await
                .context("cannot start the check sandbox")?,
        ),
        _ => None,
    };

    let started = Instant::now();
    let result = app
        .agent
        .run_until(session, task.prompt.clone(), interrupt)
        .await;
    let wall = started.elapsed();
    let finish = result
        .map(|outcome| outcome.reason)
        .map_err(|error| error.to_string());

    // The operator asked to stop, and a check can be a `cargo test` that
    // takes minutes - so none runs. A run that finished just as the key was
    // pressed is reported as interrupted too: unchecked, it has not passed.
    if interrupt.is_fired() {
        return Ok(Ran {
            finish: finish.map(|_| FinishReason::Interrupted),
            wall,
            checks: Vec::new(),
        });
    }

    let files = match LocalFileSystem::new(app.workspace.clone(), app.settings.max_file_bytes) {
        Ok(files) => files,
        // After the run, so its usage and wall time are still reported;
        // without checks it must not pass, so it is reported as not run.
        Err(error) => {
            return Ok(Ran {
                finish: Err(format!(
                    "cannot open the scratch workspace to check it: {error}"
                )),
                wall,
                checks: Vec::new(),
            });
        }
    };
    let checks = verify(
        &task.task.checks,
        &files,
        checks_runner
            .as_ref()
            .map(|runner| runner as &dyn CommandRunner),
    )
    .await;

    Ok(Ran {
        finish,
        wall,
        checks,
    })
}

/// A check's detail can quote the command's output - which is whatever the
/// model's code chose to print, the environment included - and the report
/// is a file people pass around.
fn redact_checks(checks: Vec<CheckOutcome>, redactor: &Redactor) -> Vec<CheckOutcome> {
    checks
        .into_iter()
        .map(|check| CheckOutcome {
            detail: redactor.redact(&check.detail).into_owned(),
            ..check
        })
        .collect()
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
        eprintln!("      error: {}", printable(error));
    }
    for check in report.checks.iter().filter(|check| !check.passed) {
        eprintln!("{}", failed_check_line(check));
    }
    if let Some(dir) = &report.scratch_dir {
        eprintln!("      kept: {}", printable(dir));
    }
}

/// The detail quotes output the model's code produced, which can carry
/// escape sequences meant for the operator's terminal.
fn failed_check_line(check: &CheckOutcome) -> String {
    format!(
        "      {} {}: {}",
        check.kind,
        printable(&check.target),
        printable(&check.detail)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_infrastructure::config::MapEnv;
    use agent_test_support::{MockLlmServer, Response};
    use clap::Parser as _;

    fn args(extra: &[&str], out: &Path) -> EvalArgs {
        let out = out.display().to_string();
        let mut argv = vec!["agent", "eval", "suite.json", "--out", &out];
        argv.extend_from_slice(extra);
        match Cli::parse_from(argv).command {
            Some(crate::args::Command::Eval(args)) => args,
            other => panic!("expected eval, got {other:?}"),
        }
    }

    fn settings(pairs: &[(&str, &str)]) -> Settings {
        let mut env = MapEnv::new(&[
            ("AGENT_WORKSPACE", "/workspace"),
            ("AGENT_PROVIDER", "openai"),
            ("AGENT_BASE_URL", "http://127.0.0.1:9/v1"),
            ("AGENT_MODEL", "mock"),
            ("AGENT_STREAM", "false"),
            ("AGENT_MAX_RETRIES", "0"),
        ]);
        for (key, value) in pairs {
            env.set(key, value);
        }
        Settings::from_source(&env).unwrap()
    }

    fn plan(settings: Settings, keep: bool) -> Plan {
        Plan {
            checks: None,
            settings,
            model: None,
            environment: Vec::new(),
            keep,
        }
    }

    fn task(id: &str, fixture: &Path) -> PreparedTask {
        let suite = TaskSuite::parse(&format!(
            r#"{{"version": 1, "tasks": [{{"id": "{id}", "fixture": "f", "prompt": "Write hello.txt.",
                 "checks": [{{"type": "file_contains", "path": "hello.txt", "contains": "hello"}}]}}]}}"#
        ))
        .unwrap();
        PreparedTask {
            task: suite.tasks.into_iter().next().unwrap(),
            fixture: fixture.to_path_buf(),
            prompt: "Write hello.txt.".into(),
        }
    }

    /// Settings that would reach outside the scratch copy: a web fetcher, and
    /// an MCP configuration - one that does not exist, so that assembling
    /// with it fails on every platform and shows it was attempted.
    const OUTWARD: [(&str, &str); 2] = [
        ("AGENT_WEB_FETCH", "true"),
        ("AGENT_MCP_CONFIG", "/nonexistent/mcp.json"),
    ];

    async fn assembled(adapted: Settings) -> Result<Application> {
        let workspace = tempfile::tempdir().unwrap();
        let plan = plan(adapted, false);
        let settings = plan.settings.clone();
        assemble(&plan, settings, workspace.path(), Arc::new(NullEventSink)).await
    }

    #[tokio::test]
    async fn eval_offers_neither_mcp_servers_nor_web_fetch_unless_asked() {
        let out = tempfile::tempdir().unwrap();
        let mut settings = settings(&OUTWARD);
        adapt_settings(&mut settings, &args(&[], out.path())).unwrap();

        let app = assembled(settings)
            .await
            .expect("without the MCP configuration, assembly cannot fail on it");

        assert!(
            !app.tools.names().iter().any(|name| name == "web_fetch"),
            "{:?}",
            app.tools.names()
        );
        assert!(app.tool_servers.is_empty());
    }

    #[tokio::test]
    async fn allow_web_offers_web_fetch() {
        let out = tempfile::tempdir().unwrap();
        let mut settings = settings(&[("AGENT_WEB_FETCH", "true")]);
        adapt_settings(&mut settings, &args(&["--allow-web"], out.path())).unwrap();

        let app = assembled(settings).await.unwrap();

        assert!(
            app.tools.names().iter().any(|name| name == "web_fetch"),
            "{:?}",
            app.tools.names()
        );
    }

    /// Eval approves every call by itself, so a search there would send the
    /// model's query with nobody asked.
    #[tokio::test]
    async fn eval_offers_web_search_only_with_allow_web() {
        const SEARCH: [(&str, &str); 2] = [
            ("AGENT_WEB_SEARCH", "true"),
            ("AGENT_WEB_SEARCH_API_KEY", "BSA-0123456789abcdef"),
        ];
        let offered = |app: &Application| app.tools.names().iter().any(|n| n == "web_search");

        let out = tempfile::tempdir().unwrap();
        let mut unasked = settings(&SEARCH);
        adapt_settings(&mut unasked, &args(&[], out.path())).unwrap();
        let app = assembled(unasked).await.unwrap();
        assert!(!offered(&app), "{:?}", app.tools.names());

        let mut asked = settings(&SEARCH);
        adapt_settings(&mut asked, &args(&["--allow-web"], out.path())).unwrap();
        let app = assembled(asked).await.unwrap();
        assert!(offered(&app), "{:?}", app.tools.names());
    }

    #[tokio::test]
    async fn allow_mcp_starts_the_configured_servers() {
        let out = tempfile::tempdir().unwrap();
        let mut settings = settings(&OUTWARD);
        adapt_settings(&mut settings, &args(&["--allow-mcp"], out.path())).unwrap();

        let error = assembled(settings)
            .await
            .err()
            .expect("the configuration is used, and it does not exist");

        assert!(
            format!("{error:#}").contains("AGENT_MCP_CONFIG"),
            "{error:#}"
        );
    }

    #[tokio::test]
    async fn an_attempt_that_cannot_be_assembled_is_reported_with_its_kept_directory() {
        let fixture = tempfile::tempdir().unwrap();
        let out = tempfile::tempdir().unwrap();
        // Assembly checks the file exists; the attempt is never sent.
        let mut settings = settings(&[("AGENT_GLOBAL_INSTRUCTIONS", "/nonexistent/global.md")]);
        adapt_settings(&mut settings, &args(&[], out.path())).unwrap();
        let plan = plan(settings, true);

        let report = run_attempt(
            &plan,
            &task("broken", fixture.path()),
            1,
            &Redactor::new(Vec::new()),
            &Interrupt::new(),
        )
        .await;

        assert!(!report.passed);
        assert_eq!(report.finish_reason, "error");
        let error = report.error.as_deref().unwrap_or_default();
        assert!(error.contains("cannot assemble the agent"), "{error}");
        assert!(error.contains("AGENT_GLOBAL_INSTRUCTIONS"), "{error}");
        let kept = report.scratch_dir.expect("--keep names the directory");
        assert!(Path::new(&kept).is_dir(), "{kept}");
        std::fs::remove_dir_all(kept).unwrap();
    }

    #[tokio::test]
    async fn an_attempt_that_cannot_be_set_up_does_not_cost_the_rest_of_the_suite() {
        let llm = MockLlmServer::start(vec![
            Response::tool_call(
                "call_1",
                "write_file",
                r#"{"path":"hello.txt","content":"hello\n"}"#,
            ),
            Response::assistant_text("Done."),
        ])
        .await;
        let gone = tempfile::tempdir().unwrap();
        let fixture = tempfile::tempdir().unwrap();
        let out = tempfile::tempdir().unwrap();
        let mut settings = settings(&[("AGENT_BASE_URL", llm.base_url())]);
        adapt_settings(&mut settings, &args(&[], out.path())).unwrap();
        let plan = plan(settings, false);
        // Loaded, then removed before its attempt: the copy fails.
        let tasks = [task("vanished", gone.path()), task("fine", fixture.path())];
        drop(gone);

        let (_press, mut sigint) = Sigint::fake();
        let run = run_suite(&plan, &tasks, 1, &Redactor::new(Vec::new()), &mut sigint).await;

        assert!(!run.interrupted);
        let first = &run.attempts[0][0];
        assert_eq!(first.finish_reason, "error", "{first:?}");
        assert!(
            first
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("cannot copy the fixture"),
            "{first:?}"
        );
        let second = &run.attempts[1][0];
        assert!(second.passed, "{second:?}");

        let report = SuiteReport::new(
            Fingerprint {
                agent_version: "0".into(),
                provider: "p".into(),
                model: "m".into(),
                tools: Vec::new(),
                run_command_sandbox: None,
                check_sandbox: None,
                settings: serde_json::Value::Null,
                system_prompt_fnv1a: String::new(),
            },
            1,
            tasks
                .into_iter()
                .zip(run.attempts)
                .map(|(task, attempts)| TaskReport::new(task.task.id, attempts))
                .collect(),
        );
        assert_eq!(
            exit_code(&report),
            ExitCode::FAILURE,
            "an attempt that did not run makes the suite's numbers incomplete"
        );
    }

    #[test]
    fn a_secret_in_a_check_detail_does_not_reach_the_report() {
        const SECRET: &str = "sk-test-0123456789abcdef";
        let checks = redact_checks(
            vec![CheckOutcome {
                kind: "command",
                target: "env".into(),
                passed: false,
                detail: format!("exit 1: AGENT_API_KEY={SECRET}"),
            }],
            &Redactor::new(vec![SECRET.to_string()]),
        );
        assert!(!checks[0].detail.contains(SECRET), "{}", checks[0].detail);
        assert!(checks[0].detail.starts_with("exit 1: AGENT_API_KEY="));
    }

    #[test]
    fn a_failed_check_is_printed_without_its_escape_sequences() {
        let line = failed_check_line(&CheckOutcome {
            kind: "command",
            target: "cargo test".into(),
            passed: false,
            detail: "exit 1: \u{1b}]52;c;cm0gLXJmIH4=\u{7}\u{1b}[2J".into(),
        });
        assert!(!line.contains('\u{1b}'), "{line:?}");
        assert!(!line.contains('\u{7}'), "{line:?}");
        assert!(line.contains("exit 1:"), "{line:?}");
    }
}
