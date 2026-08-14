//! Spawning, output capping and timeout - the parts that are the same on
//! every platform.
//!
//! Two safety valves live here, and both matter for an agent loop rather than
//! for a human at a terminal:
//!
//! * **The output cap keeps draining.** A command that floods stdout must not
//!   be able to wedge itself by filling the pipe while we refuse to read, so
//!   reading continues past the cap and the excess is discarded.
//! * **The timeout kills the process group.** Killing the direct child leaves
//!   a shell's grandchildren running; putting the child in its own group and
//!   signalling the group is what actually stops `sh -c 'sleep 1000 &'`.
//!
//! The child is detached into its own *session*, which does both jobs at once
//! and closes a hole that a process group alone leaves open: a child sharing
//! our session keeps the controlling terminal, and `/dev/tty` reaches it
//! directly - past the pipes we capture. Writing there would let a command
//! print whatever it liked to the user's terminal, including an imitation of
//! the approval prompt; reading there would hand it the user's keystrokes.
//! Without a controlling terminal, opening `/dev/tty` simply fails.

use std::process::Stdio;
use std::time::{Duration, Instant};

use agent_domain::error::CommandError;
use agent_domain::ports::command::CommandOutput;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;

/// Runs `command` to completion under the cap and the timeout.
///
/// The caller has already configured the sandbox, working directory and
/// environment; this only owns the mechanics.
pub(crate) async fn run_capped(
    mut command: Command,
    timeout: Duration,
    max_output_bytes: usize,
) -> Result<CommandOutput, CommandError> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    // Its own session: a new process group (so the timeout can take the whole
    // tree down) *and* no controlling terminal. `setsid` rather than
    // `process_group(0)` because it delivers both, and because calling it
    // after the child is already a group leader would fail with EPERM.
    #[cfg(unix)]
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }

    let started = Instant::now();
    let mut child = command.spawn().map_err(|error| CommandError::SpawnFailed {
        message: error.to_string(),
    })?;

    let pid = child.id();
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

    // Each stream gets half the budget, so a chatty stderr cannot crowd out
    // the stdout the model is usually after.
    let half = max_output_bytes / 2;
    let capture = async {
        let (out, err) = tokio::join!(read_capped(stdout, half), read_capped(stderr, half));
        let status = child.wait().await;
        (out, err, status)
    };

    match tokio::time::timeout(timeout, capture).await {
        Ok(((stdout, out_cut), (stderr, err_cut), status)) => {
            let status = status.map_err(|error| CommandError::Io {
                message: error.to_string(),
            })?;
            Ok(CommandOutput {
                exit_code: status.code(),
                stdout,
                stderr,
                truncated: out_cut || err_cut,
                duration: started.elapsed(),
            })
        }
        Err(_) => {
            kill_group(pid);
            Err(CommandError::TimedOut {
                seconds: timeout.as_secs(),
            })
        }
    }
}

/// Reads a stream, keeping at most `limit` bytes but draining the rest so the
/// writer never blocks on a full pipe.
async fn read_capped<R: AsyncRead + Unpin>(reader: Option<R>, limit: usize) -> (String, bool) {
    let Some(mut reader) = reader else {
        return (String::new(), false);
    };

    let mut kept: Vec<u8> = Vec::new();
    let mut chunk = [0_u8; 8192];
    let mut truncated = false;

    loop {
        match reader.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(read) => {
                if kept.len() < limit {
                    let room = limit - kept.len();
                    let take = room.min(read);
                    kept.extend_from_slice(&chunk[..take]);
                    if take < read {
                        truncated = true;
                    }
                } else {
                    truncated = true;
                }
            }
        }
    }

    (String::from_utf8_lossy(&kept).into_owned(), truncated)
}

#[cfg(unix)]
fn kill_group(pid: Option<u32>) {
    if let Some(pid) = pid {
        // Negative pid signals the whole process group.
        unsafe { libc::kill(-(pid as i32), libc::SIGKILL) };
    }
}

#[cfg(not(unix))]
fn kill_group(_pid: Option<u32>) {}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn shell(line: &str) -> Command {
        let mut command = Command::new("/bin/sh");
        command.arg("-c").arg(line);
        command
    }

    #[tokio::test]
    async fn captures_output_and_exit_code() {
        let output = run_capped(
            shell("echo out; echo err >&2; exit 3"),
            Duration::from_secs(10),
            4096,
        )
        .await
        .unwrap();

        assert_eq!(output.exit_code, Some(3));
        assert!(!output.succeeded());
        assert_eq!(output.stdout.trim(), "out");
        assert_eq!(output.stderr.trim(), "err");
        assert!(!output.truncated);
    }

    #[tokio::test]
    async fn floods_are_capped_without_wedging_the_child() {
        // Far more output than the cap: the command must still finish, which
        // only happens if we keep draining the pipe.
        let output = run_capped(
            shell("for i in $(seq 1 20000); do echo aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa; done"),
            Duration::from_secs(30),
            2048,
        )
        .await
        .unwrap();

        assert_eq!(
            output.exit_code,
            Some(0),
            "the command must run to completion"
        );
        assert!(output.truncated);
        assert!(
            output.stdout.len() <= 1024,
            "kept {} bytes",
            output.stdout.len()
        );
    }

    #[tokio::test]
    async fn a_hanging_command_times_out() {
        let error = run_capped(shell("sleep 30"), Duration::from_millis(200), 4096)
            .await
            .unwrap_err();
        assert!(matches!(error, CommandError::TimedOut { .. }), "{error:?}");
    }

    #[tokio::test]
    async fn the_timeout_takes_background_children_with_it() {
        // The shell exits immediately but leaves a child holding the pipe. If
        // we only killed the direct child, the capture would never finish and
        // this test would hang rather than report a timeout.
        let error = run_capped(shell("sleep 30 & wait"), Duration::from_millis(200), 4096)
            .await
            .unwrap_err();
        assert!(matches!(error, CommandError::TimedOut { .. }), "{error:?}");
    }

    #[tokio::test]
    async fn a_missing_program_is_a_spawn_failure() {
        let mut command = Command::new("/nonexistent/program");
        command.arg("x");
        let error = run_capped(command, Duration::from_secs(5), 4096)
            .await
            .unwrap_err();
        assert!(
            matches!(error, CommandError::SpawnFailed { .. }),
            "{error:?}"
        );
    }
}
