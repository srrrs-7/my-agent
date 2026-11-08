//! Deciding whether a task succeeded.
//!
//! Everything goes through ports. Files are read with the same
//! [`FileSystem`] the tools use, so a check sees the workspace exactly as the
//! model left it and cannot reach outside it; commands go through a
//! [`CommandRunner`], so a check that runs the model's code (`cargo test`
//! runs its `build.rs`) is confined like the model's own commands are.

use std::time::Duration;

use agent_domain::error::FsError;
use agent_domain::model::workspace::WorkspacePath;
use agent_domain::ports::command::{CommandRequest, CommandRunner};
use agent_domain::ports::file_system::FileSystem;
use serde::Serialize;

use super::task::Check;

/// Output kept from a failing command check. The end, not the start: that is
/// where a test runner or compiler says what went wrong.
const DETAIL_TAIL_BYTES: usize = 600;

/// Output collected from a check command before the runner cuts it.
const MAX_CHECK_OUTPUT_BYTES: usize = 64 * 1024;

/// How one check came out, for the report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CheckOutcome {
    /// `file_contains` or `command`, as in the suite file.
    #[serde(rename = "type")]
    pub kind: &'static str,
    /// The path or the command line the check was about.
    pub target: String,
    pub passed: bool,
    /// Why it failed, or a short confirmation when it passed.
    pub detail: String,
}

/// Runs every check, in order, and reports each one.
///
/// All of them run even after one fails: a report that says "the file was
/// written but the tests fail" is more use than one that stopped at the first
/// problem. `commands` is `None` when no runner was built, which the caller
/// only does for a task without command checks; a command check reaching here
/// without one fails rather than being skipped.
pub async fn verify(
    checks: &[Check],
    files: &dyn FileSystem,
    commands: Option<&dyn CommandRunner>,
) -> Vec<CheckOutcome> {
    let mut outcomes = Vec::with_capacity(checks.len());
    for check in checks {
        outcomes.push(match check {
            Check::FileContains { path, contains } => file_contains(files, path, contains).await,
            Check::Command { run, timeout_secs } => {
                let timeout = Duration::from_secs(
                    timeout_secs.unwrap_or(Check::DEFAULT_COMMAND_TIMEOUT_SECS),
                );
                command(commands, run, timeout).await
            }
        });
    }
    outcomes
}

async fn file_contains(files: &dyn FileSystem, path: &WorkspacePath, needle: &str) -> CheckOutcome {
    let (passed, detail) = match files.read_to_string(path).await {
        Ok(contents) if contents.contains(needle) => (true, "found".to_string()),
        Ok(contents) => (
            false,
            format!(
                "the file exists ({} bytes) but does not contain the expected text",
                contents.len()
            ),
        ),
        Err(FsError::NotFound { .. }) => (false, "the file does not exist".to_string()),
        Err(error) => (false, format!("cannot read the file: {error}")),
    };
    CheckOutcome {
        kind: "file_contains",
        target: path.display(),
        passed,
        detail,
    }
}

async fn command(
    runner: Option<&dyn CommandRunner>,
    line: &str,
    timeout: Duration,
) -> CheckOutcome {
    let outcome = |passed, detail| CheckOutcome {
        kind: "command",
        target: line.to_string(),
        passed,
        detail,
    };
    let Some(runner) = runner else {
        return outcome(false, "no command runner was available".to_string());
    };

    let request = CommandRequest {
        command: line.to_string(),
        cwd: WorkspacePath::root(),
        timeout,
        max_output_bytes: MAX_CHECK_OUTPUT_BYTES,
    };
    match runner.run(request).await {
        Ok(output) if output.succeeded() => outcome(true, "exit 0".to_string()),
        Ok(output) => {
            let status = match output.exit_code {
                Some(code) => format!("exit {code}"),
                None => "killed by a signal".to_string(),
            };
            let combined = format!("{}{}", output.stdout, output.stderr);
            let tail = tail(combined.trim_end(), DETAIL_TAIL_BYTES);
            outcome(
                false,
                if tail.is_empty() {
                    status
                } else {
                    format!("{status}: {tail}")
                },
            )
        }
        Err(error) => outcome(false, error.to_string()),
    }
}

/// The last `max_bytes` bytes of `text`, moved forward to a char boundary.
fn tail(text: &str, max_bytes: usize) -> &str {
    let mut start = text.len().saturating_sub(max_bytes);
    while !text.is_char_boundary(start) {
        start += 1;
    }
    &text[start..]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_tail_never_splits_a_character() {
        let text = "ああああ"; // 3 bytes each
        assert_eq!(tail(text, 4), "あ");
        assert_eq!(tail("short", 100), "short");
    }
}
