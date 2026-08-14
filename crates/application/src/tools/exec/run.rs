use std::sync::Arc;
use std::time::Duration;

use agent_domain::error::{CommandError, ToolError};
use agent_domain::model::tool::{ToolDefinition, ToolName, ToolOutcome, ToolSafety};
use agent_domain::model::workspace::WorkspacePath;
use agent_domain::ports::command::{CommandOutput, CommandRequest, CommandRunner};
use agent_domain::ports::tool::Tool;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::tools::util::{ToolErrorContext, parse_arguments};

/// Ceiling on what a model may ask for, so a mistyped timeout cannot park the
/// loop for an hour.
const MAX_TIMEOUT: Duration = Duration::from_secs(600);
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(120);

/// Roughly the useful upper bound for a tool result: enough for a compiler's
/// worth of errors, not enough to spend the whole context on one command.
const MAX_OUTPUT_BYTES: usize = 32 * 1024;

/// Runs a shell command inside the sandbox and reports what happened.
///
/// The description handed to the model is built at construction time from the
/// runner's *actual* [`SandboxKind`](agent_domain::ports::command::SandboxKind)
/// and allowlist rather than from configuration. A model that is told it has
/// no network stops trying to install things; one that is told the wrong thing
/// burns turns discovering the truth.
pub struct RunCommandTool {
    runner: Arc<dyn CommandRunner>,
    description: String,
}

#[derive(Debug, Deserialize)]
struct Input {
    command: String,
    /// Workspace-relative; defaults to the root.
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    timeout_seconds: Option<u64>,
}

impl RunCommandTool {
    /// `allowed_domains` is only for the description - the enforcement is the
    /// runner's, and passing it here duplicates nothing but the wording.
    pub fn new(runner: Arc<dyn CommandRunner>, allowed_domains: &[String]) -> Self {
        let description = describe(runner.as_ref(), allowed_domains);
        Self {
            runner,
            description,
        }
    }

    fn name() -> ToolName {
        ToolName::new("run_command").expect("static tool name is valid")
    }
}

fn describe(runner: &dyn CommandRunner, allowed_domains: &[String]) -> String {
    let network = if allowed_domains.is_empty() {
        "There is no network access: every outbound connection is refused, so \
         package installs and downloads will fail."
            .to_string()
    } else {
        format!(
            "Network access is limited to these domains, through a proxy: {}. \
             Anything else is refused.",
            allowed_domains.join(", ")
        )
    };

    format!(
        "Run a shell command in the workspace and return its output.\n\
         Use it for builds, tests, formatters and version control - anything the \
         file tools cannot express.\n\n\
         Sandbox: {}. Writes outside the workspace are refused by the operating \
         system, not by convention.\n\
         {network}\n\
         The command runs under `/bin/sh -c`, so pipes and redirection work. \
         Long output is truncated; narrow it yourself (`| tail -40`) rather than \
         relying on that.",
        runner.sandbox().describe()
    )
}

#[async_trait]
impl Tool for RunCommandTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: Self::name(),
            description: self.description.clone(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "description": "Shell line to run, e.g. `cargo test -p agent-domain 2>&1 | tail -40`."
                    },
                    "cwd": {
                        "type": "string",
                        "description": "Directory to run in, relative to the workspace root. Defaults to the root."
                    },
                    "timeout_seconds": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": MAX_TIMEOUT.as_secs(),
                        "description": "Seconds before the command and its children are killed. Defaults to 120."
                    }
                },
                "required": ["command"],
                "additionalProperties": false
            }),
            // Not because every command destroys something, but because
            // nothing about the arguments says whether this one does.
            safety: ToolSafety::Destructive,
        }
    }

    async fn execute(&self, arguments: Value) -> Result<ToolOutcome, ToolError> {
        let name = Self::name();
        let input: Input = parse_arguments(&name, arguments)?;

        let cwd = match input.cwd.as_deref() {
            Some(path) if !path.trim().is_empty() && path != "." => {
                WorkspacePath::parse(path).for_tool(&name)?
            }
            _ => WorkspacePath::root(),
        };

        // Clamped rather than rejected: the model asking for too long is not a
        // reason to make it guess again.
        let timeout = input
            .timeout_seconds
            .map(Duration::from_secs)
            .unwrap_or(DEFAULT_TIMEOUT)
            .clamp(Duration::from_secs(1), MAX_TIMEOUT);

        let output = self
            .runner
            .run(CommandRequest {
                command: input.command.clone(),
                cwd,
                timeout,
                max_output_bytes: MAX_OUTPUT_BYTES,
            })
            .await
            .map_err(|error| match error {
                // The model can fix these by sending different arguments.
                CommandError::Refused { .. } => ToolError::invalid_input(&name, error.to_string()),
                other => ToolError::execution(&name, other.to_string()),
            })?;

        Ok(render(&input.command, &output))
    }
}

/// A non-zero exit is a *result*, not a tool failure: the model asked what
/// happens when this runs, and "the tests failed, here is the output" is the
/// answer. Returning `Err` would throw away the output that makes it useful.
fn render(command: &str, output: &CommandOutput) -> ToolOutcome {
    let status = match output.exit_code {
        Some(0) => "exit 0".to_string(),
        Some(code) => format!("exit {code}"),
        None => "killed by a signal".to_string(),
    };

    let mut content = format!(
        "$ {command}\n[{status}, {:.1}s]\n",
        output.duration.as_secs_f64()
    );

    for (label, stream) in [("stdout", &output.stdout), ("stderr", &output.stderr)] {
        if !stream.trim().is_empty() {
            content.push_str(&format!("\n--- {label} ---\n{}", stream.trim_end()));
            content.push('\n');
        }
    }
    if output.stdout.trim().is_empty() && output.stderr.trim().is_empty() {
        content.push_str("\n(no output)\n");
    }
    if output.truncated {
        content.push_str(
            "\n[output truncated - re-run with a narrower command such as `... | tail -40`]\n",
        );
    }

    ToolOutcome::new(content).with_summary(format!("{status}: {}", first_line(command)))
}

fn first_line(command: &str) -> String {
    command.lines().next().unwrap_or("").trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_domain::ports::command::SandboxKind;
    use std::sync::Mutex;

    struct StubRunner {
        kind: SandboxKind,
        result: Mutex<Option<Result<CommandOutput, CommandError>>>,
        seen: Mutex<Option<CommandRequest>>,
    }

    impl StubRunner {
        fn ok(output: CommandOutput) -> Arc<Self> {
            Arc::new(Self {
                kind: SandboxKind::LandlockConfined,
                result: Mutex::new(Some(Ok(output))),
                seen: Mutex::new(None),
            })
        }

        fn failing(error: CommandError) -> Arc<Self> {
            Arc::new(Self {
                kind: SandboxKind::LandlockConfined,
                result: Mutex::new(Some(Err(error))),
                seen: Mutex::new(None),
            })
        }
    }

    #[async_trait]
    impl CommandRunner for StubRunner {
        fn sandbox(&self) -> SandboxKind {
            self.kind
        }

        async fn run(&self, request: CommandRequest) -> Result<CommandOutput, CommandError> {
            *self.seen.lock().unwrap() = Some(request);
            self.result
                .lock()
                .unwrap()
                .take()
                .expect("the stub was called twice")
        }
    }

    fn output(code: i32, stdout: &str, stderr: &str) -> CommandOutput {
        CommandOutput {
            exit_code: Some(code),
            stdout: stdout.into(),
            stderr: stderr.into(),
            truncated: false,
            duration: Duration::from_millis(1500),
        }
    }

    #[tokio::test]
    async fn reports_output_and_status() {
        let tool = RunCommandTool::new(StubRunner::ok(output(0, "ok\n", "")), &[]);
        let outcome = tool
            .execute(json!({"command": "cargo test"}))
            .await
            .unwrap();

        assert!(outcome.content.contains("$ cargo test"));
        assert!(outcome.content.contains("[exit 0, 1.5s]"));
        assert!(outcome.content.contains("ok"));
    }

    #[tokio::test]
    async fn a_failing_command_is_a_result_not_an_error() {
        // The model asked what happens when this runs; the failure *is* the
        // answer, and it is only useful with the output attached.
        let tool = RunCommandTool::new(StubRunner::ok(output(101, "", "test failed\n")), &[]);
        let outcome = tool
            .execute(json!({"command": "cargo test"}))
            .await
            .expect("a non-zero exit must not be a tool error");

        assert!(outcome.content.contains("[exit 101"));
        assert!(outcome.content.contains("test failed"));
    }

    #[tokio::test]
    async fn a_signal_kill_is_spelled_out() {
        let tool = RunCommandTool::new(
            StubRunner::ok(CommandOutput {
                exit_code: None,
                ..output(0, "", "")
            }),
            &[],
        );
        let outcome = tool.execute(json!({"command": "sleep 1"})).await.unwrap();
        assert!(outcome.content.contains("killed by a signal"));
        assert!(outcome.content.contains("(no output)"));
    }

    #[tokio::test]
    async fn an_absurd_timeout_is_clamped_rather_than_rejected() {
        let runner = StubRunner::ok(output(0, "", ""));
        let tool = RunCommandTool::new(runner.clone(), &[]);
        tool.execute(json!({"command": "true", "timeout_seconds": 99999}))
            .await
            .unwrap();

        let seen = runner.seen.lock().unwrap().clone().unwrap();
        assert_eq!(seen.timeout, MAX_TIMEOUT);
    }

    #[tokio::test]
    async fn the_default_cwd_is_the_workspace_root() {
        let runner = StubRunner::ok(output(0, "", ""));
        let tool = RunCommandTool::new(runner.clone(), &[]);
        tool.execute(json!({"command": "pwd", "cwd": "."}))
            .await
            .unwrap();

        let seen = runner.seen.lock().unwrap().clone().unwrap();
        assert_eq!(seen.cwd, WorkspacePath::root());
    }

    #[tokio::test]
    async fn an_escaping_cwd_is_the_models_mistake() {
        let tool = RunCommandTool::new(StubRunner::ok(output(0, "", "")), &[]);
        let error = tool
            .execute(json!({"command": "ls", "cwd": "../../etc"}))
            .await
            .unwrap_err();
        assert!(matches!(error, ToolError::InvalidInput { .. }), "{error}");
    }

    #[tokio::test]
    async fn a_refusal_reads_as_invalid_input_so_the_model_can_correct_it() {
        let tool = RunCommandTool::new(
            StubRunner::failing(CommandError::Refused {
                reason: "the command is empty".into(),
            }),
            &[],
        );
        let error = tool.execute(json!({"command": "   "})).await.unwrap_err();
        assert!(matches!(error, ToolError::InvalidInput { .. }), "{error}");
    }

    #[tokio::test]
    async fn a_timeout_is_an_execution_error() {
        let tool = RunCommandTool::new(
            StubRunner::failing(CommandError::TimedOut { seconds: 120 }),
            &[],
        );
        let error = tool
            .execute(json!({"command": "sleep 999"}))
            .await
            .unwrap_err();
        assert!(matches!(error, ToolError::Execution { .. }), "{error}");
    }

    #[test]
    fn the_description_states_the_sandbox_and_the_allowlist() {
        let tool = RunCommandTool::new(
            StubRunner::ok(output(0, "", "")),
            &["crates.io".to_string()],
        );
        let description = tool.definition().description;
        assert!(description.contains("Landlock"), "{description}");
        assert!(description.contains("crates.io"), "{description}");
    }

    #[test]
    fn a_model_with_no_network_is_told_so_plainly() {
        let tool = RunCommandTool::new(StubRunner::ok(output(0, "", "")), &[]);
        let description = tool.definition().description;
        assert!(description.contains("no network access"), "{description}");
    }

    #[test]
    fn the_tool_is_destructive_and_never_auto_runs() {
        let tool = RunCommandTool::new(StubRunner::ok(output(0, "", "")), &[]);
        let definition = tool.definition();
        assert_eq!(definition.safety, ToolSafety::Destructive);
        assert!(!definition.safety.is_read_only());
    }
}
