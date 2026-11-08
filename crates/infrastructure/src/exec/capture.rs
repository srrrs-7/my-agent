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
//! * **So does being abandoned.** When the caller drops the future - the turn
//!   was interrupted - the group is killed as well, not only the direct child
//!   (see [`super::process`]).
//! * **And so does finishing.** `(sleep 20; echo x >> Cargo.toml) >/dev/null
//!   2>&1 &` lets the shell exit at once with the subshell holding no pipe, so the
//!   capture ends normally while the subshell is still waiting to act - after
//!   the dependency check has compared the workspace and the call has been
//!   reported as done. Whatever is left in the group when the command ends is
//!   therefore killed too.
//!
//! ## Kill, then reap
//!
//! The group is signalled through the leader's pid, and that is only safe
//! while the leader is unreaped: once reaped, its pid - and the group id with
//! it, as soon as the group empties - may be handed to an unrelated process.
//! So the order has to be "notice the end, kill the group, reap", and tokio's
//! `Child::wait` cannot provide the middle step: it reports the exit only by
//! reaping. A non-reaping `waitid(WNOWAIT)` is not something tokio exposes,
//! and calling it by hand on a pid the runtime owns would mean blocking a
//! thread on it and keeping that thread in step with the runtime's reaper.
//!
//! The end is noticed instead through the pipes. Both reach EOF only when
//! every process holding a write end has closed it - for the leader that
//! means it has exited (or deliberately closed both), and a background child
//! that kept them would hold the capture open until the timeout, which kills
//! the group anyway. At EOF the leader is at worst a zombie, still owning its
//! pid, so the group is killed and only then reaped.
//!
//! The cost: a leader that closes its own stdout and stderr and keeps working
//! is killed at that point. That is a command detaching itself from the
//! capture, which is the behaviour being shut down here.
//!
//! A process that left the group - `setsid` or `setpgid` - is beyond a group
//! signal, so the group kill is followed by a sweep for those
//! ([`super::reaper`]), and the call returns only once everything it found
//! has been reaped. Where the platform offers no way to find them, they can
//! outlive the call; `agent doctor` and the tool description say so.

use std::process::Stdio;
use std::time::{Duration, Instant};

use agent_domain::error::CommandError;
use agent_domain::ports::command::CommandOutput;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;

use super::process::{GuardedChild, kill_group};
use super::reaper;

/// Runs `command` to completion under the cap and the timeout.
///
/// The caller has already configured the sandbox, working directory and
/// environment; this only owns the mechanics.
pub(crate) async fn run_capped(
    command: Command,
    timeout: Duration,
    max_output_bytes: usize,
) -> Result<CommandOutput, CommandError> {
    run_capped_with_input(command, None, timeout, max_output_bytes).await
}

/// [`run_capped`], with `input` written to the child's stdin and then closed.
/// `None` gives the child no stdin at all, as a command run for the model
/// gets.
///
/// The input is written alongside the reading, never before it: a child that
/// prints before it reads would otherwise fill its pipe while we wait for it
/// to take the input, and neither side would move. A child that exits without
/// reading it all is not an error - what it chose to ignore is its business.
pub(crate) async fn run_capped_with_input(
    mut command: Command,
    input: Option<Vec<u8>>,
    timeout: Duration,
    max_output_bytes: usize,
) -> Result<CommandOutput, CommandError> {
    command
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let started = Instant::now();
    let mut guarded = GuardedChild::spawn(command)?;

    let pid = guarded.child.id();
    let stdin = guarded.child.stdin.take();
    let stdout = guarded.child.stdout.take();
    let stderr = guarded.child.stderr.take();

    // Each stream gets half the budget, so a chatty stderr cannot crowd out
    // the stdout the model is usually after.
    let half = max_output_bytes / 2;
    let capture = async {
        let (_, out, err) = tokio::join!(
            write_input(stdin, input),
            read_capped(stdout, half),
            read_capped(stderr, half)
        );
        // Before `wait`, which reaps: see "Kill, then reap" above.
        kill_group(pid);
        let found = reaper::sweep(pid.as_slice());
        let status = guarded.child.wait().await;
        guarded.group.disarm();
        (out, err, status, found)
    };

    match tokio::time::timeout(timeout, capture).await {
        Ok(((stdout, out_cut), (stderr, err_cut), status, found)) => {
            // Before returning: the caller's dependency check compares the
            // workspace next, and nothing this command started may act after.
            settle(found).await;
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
            settle(guarded.group.stop()).await;
            Err(CommandError::TimedOut {
                seconds: timeout.as_secs(),
            })
        }
    }
}

/// Waits, off the runtime, for the swept processes to be reaped.
async fn settle(found: std::collections::HashSet<i32>) {
    // A failed join only means the wait was abandoned; everything found has
    // been signalled already.
    let _ = tokio::task::spawn_blocking(move || reaper::settle(found, reaper::SETTLE_BUDGET)).await;
}

/// Writes `input` and closes the pipe, so the child sees end of file.
async fn write_input(stdin: Option<tokio::process::ChildStdin>, input: Option<Vec<u8>>) {
    let (Some(mut stdin), Some(input)) = (stdin, input) else {
        return;
    };
    // A broken pipe means the child stopped reading; its exit status says
    // the rest.
    let _ = stdin.write_all(&input).await;
    let _ = stdin.shutdown().await;
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

    /// Whether `pid` is still a process we could signal.
    fn alive(pid: i32) -> bool {
        // Signal 0 checks for existence without delivering anything.
        unsafe { libc::kill(pid, 0) == 0 }
    }

    /// Abandoning the run - what an interrupted turn does - must take the
    /// whole group down, not only the shell. The background `sleep` is the
    /// grandchild a plain `kill_on_drop` leaves behind.
    #[tokio::test]
    async fn dropping_the_run_kills_the_grandchildren_too() {
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("grandchild.pid");
        let line = format!(
            "sleep 100 & echo $! > {}.tmp && mv {0}.tmp {0}; wait",
            pid_file.display()
        );
        let mut run = Box::pin(run_capped(shell(&line), Duration::from_secs(60), 4096));

        // Bounded poll on process state, driving the run while we wait: the
        // pid file appears (atomically, via `mv`) once the grandchild exists.
        let deadline = Instant::now() + Duration::from_secs(20);
        let grandchild = loop {
            if let Ok(text) = std::fs::read_to_string(&pid_file) {
                break text.trim().parse::<i32>().expect("a pid");
            }
            assert!(
                Instant::now() < deadline,
                "the shell never started its child"
            );
            tokio::select! {
                _ = &mut run => panic!("the run finished on its own"),
                _ = tokio::time::sleep(Duration::from_millis(10)) => {}
            }
        };
        assert!(alive(grandchild), "the grandchild should be running");

        drop(run);

        // SIGKILL is asynchronous; the grandchild is reparented to init and
        // reaped there, so it may take a moment to disappear.
        let deadline = Instant::now() + Duration::from_secs(20);
        while alive(grandchild) {
            assert!(
                Instant::now() < deadline,
                "grandchild {grandchild} survived the dropped run"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ESRCH),
            "gone, not merely unsignalable"
        );
    }

    /// A command that finished normally must not leave a background child
    /// behind: one that holds no pipe would otherwise outlive the call and
    /// act after it was reported as done.
    #[tokio::test]
    async fn a_finished_command_leaves_no_background_child_behind() {
        let output = run_capped(
            shell("sleep 30 >/dev/null 2>&1 & echo $!"),
            Duration::from_secs(20),
            4096,
        )
        .await
        .unwrap();
        assert_eq!(output.exit_code, Some(0), "{output:?}");
        let grandchild: i32 = output.stdout.trim().parse().expect("a pid");

        // SIGKILL is asynchronous and the orphan is reaped by init, so give
        // it a bounded moment to disappear.
        let deadline = Instant::now() + Duration::from_secs(20);
        while alive(grandchild) {
            assert!(
                Instant::now() < deadline,
                "background child {grandchild} survived the finished command"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ESRCH),
            "gone, not merely unsignalable"
        );
    }

    #[tokio::test]
    async fn input_reaches_the_child_and_is_closed() {
        // `cat` stops only at end of file, so finishing at all proves the
        // pipe was closed after the write.
        let output = run_capped_with_input(
            shell("cat; echo done >&2"),
            Some(b"{\"v\":1}".to_vec()),
            Duration::from_secs(10),
            4096,
        )
        .await
        .unwrap();
        assert_eq!(output.stdout, "{\"v\":1}");
        assert_eq!(output.stderr.trim(), "done");
    }

    #[tokio::test]
    async fn a_child_that_ignores_a_large_input_still_finishes() {
        // Larger than a pipe buffer: the write cannot complete, and must not
        // hold the capture open once the child has gone.
        let output = run_capped_with_input(
            shell("exit 4"),
            Some(vec![b'x'; 1024 * 1024]),
            Duration::from_secs(10),
            4096,
        )
        .await
        .unwrap();
        assert_eq!(output.exit_code, Some(4), "{output:?}");
    }

    #[tokio::test]
    async fn the_exit_code_survives_the_group_kill() {
        let output = run_capped(shell("exit 7"), Duration::from_secs(10), 4096)
            .await
            .unwrap();
        assert_eq!(output.exit_code, Some(7), "{output:?}");
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
