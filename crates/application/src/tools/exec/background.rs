//! Background jobs: `start_command`, `read_output` and `kill_command`.
//!
//! Three tools rather than one with a mode, so each carries the safety class
//! of what it does: starting a command is [`ToolSafety::Destructive`] like
//! `run_command` (the human approves the literal line), reading its output
//! changes nothing, and stopping one is [`ToolSafety::Mutating`] - it ends
//! something, but only something the agent itself started.
//!
//! The job's effects on dependency files cannot be compared when
//! `start_command` returns - the job has barely begun - so the supervisor
//! compares the workspace across the job's whole life and hands the result
//! out once, with the first read that observes the end. It goes at the head
//! of that result, where `run_command`'s note goes, and to the terminal as
//! [`AgentEvent::DependenciesChanged`].

use std::sync::Arc;
use std::time::Duration;

use agent_domain::error::{CommandError, JobError, ToolError};
use agent_domain::model::tool::{PlannedWrite, ToolDefinition, ToolName, ToolOutcome, ToolSafety};
use agent_domain::model::workspace::WorkspacePath;
use agent_domain::ports::background::{BackgroundCommands, JobId, JobOutput, JobStop};
use agent_domain::ports::command::CommandRequest;
use agent_domain::ports::events::{AgentEvent, EventSink};
use agent_domain::ports::tool::Tool;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::tools::util::{ToolErrorContext, parse_arguments};

/// Bytes of output one read returns by default, and at most.
const DEFAULT_READ_BYTES: usize = 16 * 1024;
const MAX_READ_BYTES: usize = 32 * 1024;

fn start_name() -> ToolName {
    ToolName::new("start_command").expect("static tool name is valid")
}

fn read_name() -> ToolName {
    ToolName::new("read_output").expect("static tool name is valid")
}

fn kill_name() -> ToolName {
    ToolName::new("kill_command").expect("static tool name is valid")
}

/// The model's mistakes read as invalid input, so it corrects its arguments;
/// the rest are failures it can only work around.
fn job_error(name: &ToolName, error: JobError) -> ToolError {
    match error {
        JobError::Command(CommandError::Refused { .. }) | JobError::Unknown { .. } => {
            ToolError::invalid_input(name, error.to_string())
        }
        other => ToolError::execution(name, other.to_string()),
    }
}

/// Starts a command in the background and returns its job id at once.
pub struct StartCommandTool {
    jobs: Arc<dyn BackgroundCommands>,
    description: String,
}

#[derive(Debug, Deserialize)]
struct StartInput {
    command: String,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    lifetime_seconds: Option<u64>,
}

impl StartCommandTool {
    /// `allowed_domains` is only for the description, as for `run_command`.
    pub fn new(jobs: Arc<dyn BackgroundCommands>, allowed_domains: &[String]) -> Self {
        let limits = jobs.limits();
        let network = if allowed_domains.is_empty() {
            "There is no network access.".to_string()
        } else {
            format!(
                "Network access is limited to these domains, through a proxy: {}.",
                allowed_domains.join(", ")
            )
        };
        let description = format!(
            "Start a shell command in the background and return a job id at once, without \
             waiting for it to finish. Use it for dev servers, watchers and long test runs; use \
             `run_command` for anything that finishes on its own within a few minutes.\n\n\
             Sandbox: {}, the same as `run_command`. {network}\n\
             Limits: at most {} jobs at a time; each is killed after {}s (pass \
             `lifetime_seconds` for less); only the last {} KiB of each job's output is kept.\n\
             Read the output with `read_output` and stop the job with `kill_command`. Jobs \
             keep running between your turns, and are all stopped when the session resets or \
             the agent exits. Changes a job makes are not undone by `/undo`.",
            jobs.sandbox().describe(),
            limits.max_jobs,
            limits.max_lifetime.as_secs(),
            (limits.buffer_bytes / 1024).max(1),
        );
        Self { jobs, description }
    }
}

#[async_trait]
impl Tool for StartCommandTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: start_name(),
            description: self.description.clone(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "description": "Shell line to run under `/bin/sh -c`, e.g. `npm run dev`."
                    },
                    "cwd": {
                        "type": "string",
                        "description": "Directory to run in, relative to the workspace root. Defaults to the root."
                    },
                    "lifetime_seconds": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": self.jobs.limits().max_lifetime.as_secs(),
                        "description": "Seconds before the job is killed. Defaults to the maximum."
                    }
                },
                "required": ["command"],
                "additionalProperties": false
            }),
            // As for `run_command`: nothing about the arguments says what the
            // command does.
            safety: ToolSafety::Destructive,
        }
    }

    async fn preview(&self, _arguments: &Value) -> Result<Vec<PlannedWrite>, ToolError> {
        // What a command writes cannot be read off its arguments. An empty
        // plan is also what makes `/undo` list the call as not taken back.
        Ok(Vec::new())
    }

    async fn execute(&self, arguments: Value) -> Result<ToolOutcome, ToolError> {
        let name = start_name();
        let input: StartInput = parse_arguments(&name, arguments)?;
        let cwd = match input.cwd.as_deref() {
            Some(path) if !path.trim().is_empty() && path != "." => {
                WorkspacePath::parse(path).for_tool(&name)?
            }
            _ => WorkspacePath::root(),
        };
        let limits = self.jobs.limits();
        // Clamped rather than rejected, like `run_command`'s timeout.
        let lifetime = input
            .lifetime_seconds
            .map(Duration::from_secs)
            .unwrap_or(limits.max_lifetime)
            .clamp(Duration::from_secs(1), limits.max_lifetime);

        let id = self
            .jobs
            .start(CommandRequest {
                command: input.command.clone(),
                cwd,
                timeout: lifetime,
                max_output_bytes: limits.buffer_bytes,
            })
            .await
            .map_err(|error| job_error(&name, error))?;

        let first = first_line(&input.command);
        Ok(ToolOutcome::new(format!(
            "Started background job {id}: $ {}\nIt is killed after {}s if it has not ended. \
             Read its output with `read_output` (job_id {id}), and stop it with \
             `kill_command` when you no longer need it.",
            input.command,
            lifetime.as_secs()
        ))
        .with_summary(format!("job {id}: {first}")))
    }
}

/// Reads what a job has written since the last read.
pub struct ReadOutputTool {
    jobs: Arc<dyn BackgroundCommands>,
    events: Arc<dyn EventSink>,
}

#[derive(Debug, Deserialize)]
struct ReadInput {
    job_id: u64,
    #[serde(default)]
    since: Option<u64>,
    #[serde(default)]
    max_bytes: Option<usize>,
}

impl ReadOutputTool {
    pub fn new(jobs: Arc<dyn BackgroundCommands>, events: Arc<dyn EventSink>) -> Self {
        Self { jobs, events }
    }
}

#[async_trait]
impl Tool for ReadOutputTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: read_name(),
            description: "Read the output of a background job started with `start_command` - \
                          stdout and stderr interleaved - and whether it is still running. \
                          Returns at once; it never waits for more output. Pass the `next` \
                          offset from the previous read as `since` to get only what is new."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "job_id": {
                        "type": "integer",
                        "description": "The id `start_command` returned."
                    },
                    "since": {
                        "type": "integer",
                        "minimum": 0,
                        "description": "Byte offset to read from: the `next` of your previous read. Defaults to 0, the oldest output still kept."
                    },
                    "max_bytes": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": MAX_READ_BYTES,
                        "description": "Most bytes to return. Defaults to 16384."
                    }
                },
                "required": ["job_id"],
                "additionalProperties": false
            }),
            safety: ToolSafety::ReadOnly,
        }
    }

    async fn preview(&self, _arguments: &Value) -> Result<Vec<PlannedWrite>, ToolError> {
        Ok(Vec::new())
    }

    async fn execute(&self, arguments: Value) -> Result<ToolOutcome, ToolError> {
        let name = read_name();
        let input: ReadInput = parse_arguments(&name, arguments)?;
        let id = JobId::new(input.job_id);
        let max = clamp_read(input.max_bytes);
        let output = self
            .jobs
            .read(id, input.since.unwrap_or(0), max)
            .await
            .map_err(|error| job_error(&name, error))?;
        Ok(render(id, &output, self.events.as_ref()))
    }
}

/// Stops a job and everything it started.
pub struct KillCommandTool {
    jobs: Arc<dyn BackgroundCommands>,
    events: Arc<dyn EventSink>,
}

#[derive(Debug, Deserialize)]
struct KillInput {
    job_id: u64,
}

impl KillCommandTool {
    pub fn new(jobs: Arc<dyn BackgroundCommands>, events: Arc<dyn EventSink>) -> Self {
        Self { jobs, events }
    }
}

#[async_trait]
impl Tool for KillCommandTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: kill_name(),
            description: "Stop a background job started with `start_command`, together with \
                          every process it started, and return the end of its output. Stopping \
                          a job that has already ended is harmless."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "job_id": {
                        "type": "integer",
                        "description": "The id `start_command` returned."
                    }
                },
                "required": ["job_id"],
                "additionalProperties": false
            }),
            // It ends something, but only something this agent started.
            safety: ToolSafety::Mutating,
        }
    }

    async fn preview(&self, _arguments: &Value) -> Result<Vec<PlannedWrite>, ToolError> {
        Ok(Vec::new())
    }

    async fn execute(&self, arguments: Value) -> Result<ToolOutcome, ToolError> {
        let name = kill_name();
        let input: KillInput = parse_arguments(&name, arguments)?;
        let id = JobId::new(input.job_id);
        let output = self
            .jobs
            .kill(id, DEFAULT_READ_BYTES)
            .await
            .map_err(|error| job_error(&name, error))?;
        Ok(render(id, &output, self.events.as_ref()))
    }
}

fn clamp_read(requested: Option<usize>) -> usize {
    requested
        .unwrap_or(DEFAULT_READ_BYTES)
        .clamp(1, MAX_READ_BYTES)
}

/// One read, as the model sees it: the dependency note first (where
/// truncation never reaches), then the job's state, then the output.
fn render(id: JobId, output: &JobOutput, events: &dyn EventSink) -> ToolOutcome {
    let status = match (output.running, output.stopped, output.exit_code) {
        (true, _, _) => "running".to_string(),
        (false, Some(JobStop::Killed), _) => "killed".to_string(),
        (false, Some(JobStop::LifetimeExceeded { seconds }), _) => {
            format!("killed after reaching its {seconds}s lifetime")
        }
        (false, None, Some(code)) => format!("exited with {code}"),
        (false, None, None) => "killed by a signal".to_string(),
    };

    let mut content = String::new();
    if let Some(report) = &output.dependencies {
        // Attributed to the tool that started the job: it is the one that
        // was approved, and the one the human recognises.
        if !report.changes.is_empty() {
            events.emit(AgentEvent::DependenciesChanged {
                name: start_name(),
                files: report.changes.clone(),
            });
        }
        if !report.complete {
            events.emit(AgentEvent::DependencyCheckIncomplete {
                name: start_name(),
                limit: report.limit,
            });
        }
        if let Some(note) = report.note(&format!("while background job {id} ran")) {
            content.push_str(&note);
            content.push_str("\n\n");
        }
    }

    content.push_str(&format!(
        "[job {id}: {status}; next since: {}]\n",
        output.next
    ));
    if output.dropped > 0 {
        content.push_str(&format!(
            "[{} bytes of output before this were discarded - only the newest output is kept]\n",
            output.dropped
        ));
    }
    if output.chunk.is_empty() {
        content.push_str(if output.running {
            "(no new output yet)\n"
        } else {
            "(no further output)\n"
        });
    } else {
        content.push_str(&output.chunk);
        if !output.chunk.ends_with('\n') {
            content.push('\n');
        }
    }

    ToolOutcome::new(content).with_summary(format!("job {id}: {status}"))
}

fn first_line(command: &str) -> String {
    command.lines().next().unwrap_or("").trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_domain::model::dependency::{
        DependencyFileChange, DependencyReport, FileChange, ManifestKind,
    };
    use agent_domain::ports::background::JobLimits;
    use agent_domain::ports::command::SandboxKind;
    use std::sync::Mutex;

    #[derive(Default)]
    struct StubJobs {
        started: Mutex<Vec<CommandRequest>>,
        read: Mutex<Option<Result<JobOutput, JobError>>>,
        start_error: Mutex<Option<JobError>>,
    }

    #[async_trait]
    impl BackgroundCommands for StubJobs {
        fn sandbox(&self) -> SandboxKind {
            SandboxKind::LandlockConfined
        }

        fn limits(&self) -> JobLimits {
            JobLimits {
                max_jobs: 2,
                max_lifetime: Duration::from_secs(600),
                buffer_bytes: 64 * 1024,
            }
        }

        async fn start(&self, request: CommandRequest) -> Result<JobId, JobError> {
            if let Some(error) = self.start_error.lock().unwrap().take() {
                return Err(error);
            }
            self.started.lock().unwrap().push(request);
            Ok(JobId::new(7))
        }

        async fn read(&self, _id: JobId, _since: u64, _max: usize) -> Result<JobOutput, JobError> {
            self.read.lock().unwrap().take().expect("one read")
        }

        async fn kill(&self, id: JobId, max: usize) -> Result<JobOutput, JobError> {
            self.read(id, 0, max).await
        }
    }

    #[derive(Default)]
    struct Recorder(Mutex<Vec<AgentEvent>>);

    impl EventSink for Recorder {
        fn emit(&self, event: AgentEvent) {
            self.0.lock().unwrap().push(event);
        }
    }

    fn ended(report: Option<DependencyReport>) -> JobOutput {
        JobOutput {
            chunk: "done\n".into(),
            next: 5,
            dropped: 0,
            running: false,
            exit_code: Some(0),
            stopped: None,
            dependencies: report,
        }
    }

    #[tokio::test]
    async fn a_lifetime_beyond_the_cap_is_clamped() {
        let jobs = Arc::new(StubJobs::default());
        let tool = StartCommandTool::new(jobs.clone(), &[]);
        let outcome = tool
            .execute(json!({"command": "npm run dev", "lifetime_seconds": 99999}))
            .await
            .unwrap();
        assert!(
            outcome.content.contains("background job 7"),
            "{}",
            outcome.content
        );
        let started = jobs.started.lock().unwrap();
        assert_eq!(started[0].timeout, Duration::from_secs(600));
    }

    #[tokio::test]
    async fn the_concurrency_cap_tells_the_model_what_to_do() {
        let jobs = Arc::new(StubJobs::default());
        *jobs.start_error.lock().unwrap() = Some(JobError::TooMany { limit: 2 });
        let error = StartCommandTool::new(jobs, &[])
            .execute(json!({"command": "sleep 100"}))
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("kill_command"), "{error}");
    }

    #[tokio::test]
    async fn an_unknown_job_is_the_models_mistake() {
        let jobs = Arc::new(StubJobs::default());
        *jobs.read.lock().unwrap() = Some(Err(JobError::Unknown { id: 9 }));
        let error = ReadOutputTool::new(jobs, Arc::new(Recorder::default()))
            .execute(json!({"job_id": 9}))
            .await
            .unwrap_err();
        assert!(matches!(error, ToolError::InvalidInput { .. }), "{error}");
    }

    #[tokio::test]
    async fn a_dependency_change_leads_the_result_and_reaches_the_terminal() {
        let jobs = Arc::new(StubJobs::default());
        let change = DependencyFileChange {
            path: WorkspacePath::parse("package.json").unwrap(),
            kind: ManifestKind::PackageJson,
            change: FileChange::Modified,
        };
        *jobs.read.lock().unwrap() = Some(Ok(ended(Some(DependencyReport {
            changes: vec![change.clone()],
            complete: true,
            limit: 100,
        }))));
        let events = Arc::new(Recorder::default());
        let outcome = ReadOutputTool::new(jobs, events.clone())
            .execute(json!({"job_id": 7}))
            .await
            .unwrap();

        assert!(
            outcome
                .content
                .starts_with("[dependency files changed while background job 7 ran: package.json"),
            "{}",
            outcome.content
        );
        let recorded = events.0.lock().unwrap();
        assert!(
            matches!(
                recorded.as_slice(),
                [AgentEvent::DependenciesChanged { name, files }]
                    if name.as_str() == "start_command" && files.as_slice() == std::slice::from_ref(&change)
            ),
            "{recorded:?}"
        );
    }

    #[tokio::test]
    async fn the_lifetime_kill_is_spelled_out() {
        let jobs = Arc::new(StubJobs::default());
        *jobs.read.lock().unwrap() = Some(Ok(JobOutput {
            exit_code: None,
            stopped: Some(JobStop::LifetimeExceeded { seconds: 60 }),
            dropped: 1000,
            ..ended(None)
        }));
        let outcome = KillCommandTool::new(jobs, Arc::new(Recorder::default()))
            .execute(json!({"job_id": 7}))
            .await
            .unwrap();
        assert!(
            outcome
                .content
                .contains("killed after reaching its 60s lifetime"),
            "{}",
            outcome.content
        );
        assert!(
            outcome.content.contains("1000 bytes"),
            "{}",
            outcome.content
        );
    }

    #[test]
    fn each_tool_carries_the_safety_of_what_it_does() {
        let jobs: Arc<dyn BackgroundCommands> = Arc::new(StubJobs::default());
        let events: Arc<dyn EventSink> = Arc::new(Recorder::default());
        assert_eq!(
            StartCommandTool::new(jobs.clone(), &[]).definition().safety,
            ToolSafety::Destructive
        );
        assert_eq!(
            ReadOutputTool::new(jobs.clone(), events.clone())
                .definition()
                .safety,
            ToolSafety::ReadOnly
        );
        assert_eq!(
            KillCommandTool::new(jobs, events).definition().safety,
            ToolSafety::Mutating
        );
    }

    #[tokio::test]
    async fn starting_a_job_plans_no_writes() {
        let tool = StartCommandTool::new(Arc::new(StubJobs::default()), &[]);
        assert!(
            tool.preview(&json!({"command": "touch x"}))
                .await
                .unwrap()
                .is_empty()
        );
    }
}
