//! Building a sandboxed `Command`, independent of how it is then run.
//!
//! A one-shot shell command and a long-lived server need the same
//! confinement - scrubbed environment, scratch directory, proxy plumbing and
//! the kernel rules - but differ in everything around it: stdio, lifetime,
//! what is captured. This builds the first part and leaves the second to the
//! caller, who spawns with [`super::process::GuardedChild::spawn`].
//!
//! The policy is spelled out by the caller rather than derived from the
//! workspace, because not every child gets the workspace: a server process
//! that outlives a single call is given its own directory instead.

use std::path::PathBuf;

use agent_domain::error::CommandError;
use agent_domain::ports::command::SandboxKind;
use tokio::process::Command;

use super::env;

/// Variables proxy-aware tools read to find their way out.
const PROXY_VARS: [&str; 6] = [
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "ALL_PROXY",
    "http_proxy",
    "https_proxy",
    "all_proxy",
];

/// The confinement one child is given.
#[derive(Debug, Clone)]
pub(crate) struct ConfinementPolicy {
    /// The mechanism actually in force - the outcome of
    /// [`super::resolve_sandbox`], never the operator's wish.
    pub kind: SandboxKind,
    /// Every root the child may write to. Nothing is added implicitly, so
    /// `scratch` belongs in here too.
    pub writable: Vec<PathBuf>,
    /// Where `TMPDIR` and friends point.
    pub scratch: PathBuf,
    /// Loopback port of this child's egress proxy. `None` means no network:
    /// no proxy variables, and no connectable port.
    pub proxy_port: Option<u16>,
}

/// Confines `command`, which the caller has already given its program,
/// arguments and working directory.
///
/// Taking a `Command` rather than describing one keeps both shapes callers
/// need - a `/bin/sh -c` line and a program with arguments - without a type
/// for the distinction. Stdio and spawning are left to the caller. Fails with
/// [`CommandError::SandboxUnavailable`] when `policy.kind` cannot be applied
/// here, so asking for confinement never yields an unconfined child.
pub(crate) fn confine_command(
    mut command: Command,
    policy: &ConfinementPolicy,
) -> Result<Command, CommandError> {
    env::scrub(&mut command);

    // Point every temp convention at the scratch directory the sandbox
    // actually allows, or the first thing a toolchain does is fail.
    for name in ["TMPDIR", "TMP", "TEMP"] {
        command.env(name, &policy.scratch);
    }

    // Proxy-aware tools pick these up and route through the allowlist.
    // An inherited `NO_PROXY` would carve holes in it, so it goes either
    // way - with no proxy there is nothing for it to except.
    for name in ["NO_PROXY", "no_proxy"] {
        command.env_remove(name);
    }
    match policy.proxy_port {
        Some(port) => {
            let url = format!("http://127.0.0.1:{port}");
            for name in PROXY_VARS {
                command.env(name, &url);
            }
        }
        None => {
            for name in PROXY_VARS {
                command.env_remove(name);
            }
        }
    }

    apply_sandbox(&mut command, policy)?;
    Ok(command)
}

fn apply_sandbox(command: &mut Command, policy: &ConfinementPolicy) -> Result<(), CommandError> {
    match policy.kind {
        SandboxKind::None => Ok(()),
        #[cfg(target_os = "linux")]
        SandboxKind::LandlockConfined => super::linux::confine(
            command,
            &super::linux::LinuxSandboxPolicy {
                writable: policy.writable.clone(),
                proxy_port: policy.proxy_port,
            },
        ),
        // Detection never reports these here, but a policy naming one must
        // not quietly run unconfined.
        unsupported => {
            // Only the Linux arm touches it; keeps other targets warning-free.
            let _ = command;
            Err(CommandError::SandboxUnavailable {
                reason: format!(
                    "the `{}` sandbox is not implemented on this platform",
                    unsupported.as_str()
                ),
            })
        }
    }
}

/// Real processes, real kernel rules - see `tests/sandbox.rs` for why a
/// ruleset that merely builds proves nothing.
#[cfg(all(test, target_os = "linux"))]
mod tests {
    use std::path::Path;
    use std::process::Stdio;
    use std::time::{Duration, Instant};

    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    use super::*;
    use crate::exec::process::GuardedChild;

    /// Reads stdin once, reports whether each root took a write, then leaves a
    /// grandchild behind and waits on it - a stand-in for a server that stays
    /// up between requests.
    const PROBE: &str = r#"
read line
echo "got=$line"
if echo ok > "$1/probe" 2>/dev/null; then echo own=ok; else echo own=denied; fi
if echo pwned > "$2/escape" 2>/dev/null; then echo workspace=ok; else echo workspace=denied; fi
sleep 100 >/dev/null 2>&1 &
echo "pid=$!"
wait
"#;

    fn canonical_tempdir() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = std::fs::canonicalize(dir.path()).unwrap();
        (dir, path)
    }

    /// A long-lived child with piped stdio, as a server would get.
    fn spawn_probe(kind: SandboxKind, own: &Path, workspace: &Path) -> GuardedChild {
        let policy = ConfinementPolicy {
            kind,
            writable: vec![own.to_path_buf()],
            scratch: own.to_path_buf(),
            proxy_port: None,
        };
        let mut command = Command::new("/bin/sh");
        command
            .arg("-c")
            .arg(PROBE)
            .arg("probe")
            .arg(own)
            .arg(workspace)
            .current_dir(own);
        let mut command = confine_command(command, &policy).expect("the sandbox builds");
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        GuardedChild::spawn(command).expect("the probe spawns")
    }

    /// Sends one line, collects the four reported lines.
    async fn converse(guarded: &mut GuardedChild) -> Vec<String> {
        let mut stdin = guarded.child.stdin.take().unwrap();
        stdin.write_all(b"hello\n").await.unwrap();
        stdin.flush().await.unwrap();

        let mut lines = BufReader::new(guarded.child.stdout.take().unwrap()).lines();
        let mut seen = Vec::new();
        while seen.len() < 4 {
            let next = tokio::time::timeout(Duration::from_secs(20), lines.next_line())
                .await
                .expect("the probe stopped talking")
                .unwrap()
                .expect("the probe closed stdout early");
            seen.push(next);
        }
        seen
    }

    fn alive(pid: i32) -> bool {
        // Signal 0 checks for existence without delivering anything.
        unsafe { libc::kill(pid, 0) == 0 }
    }

    #[tokio::test]
    async fn a_long_lived_child_writes_only_its_own_root_and_dies_with_its_guard() {
        // Like the fixture in `tests/sandbox.rs`: the dev container is the
        // reference environment, so missing Landlock is a loud failure rather
        // than a silent skip.
        let available = super::super::detect_sandbox();
        assert_eq!(
            available.kind,
            SandboxKind::LandlockConfined,
            "shortfall: {:?}",
            available.shortfall
        );

        let (_own_dir, own) = canonical_tempdir();
        let (_ws_dir, workspace) = canonical_tempdir();

        // Control: unconfined, the same probe can write the workspace, so a
        // `denied` below is the kernel and not a broken probe.
        let mut control = spawn_probe(SandboxKind::None, &own, &workspace);
        let reported = converse(&mut control).await;
        assert!(
            reported.contains(&"workspace=ok".to_string()),
            "the control write must succeed: {reported:?}"
        );
        drop(control);
        std::fs::remove_file(workspace.join("escape")).unwrap();
        std::fs::remove_file(own.join("probe")).unwrap();

        let mut guarded = spawn_probe(SandboxKind::LandlockConfined, &own, &workspace);
        let reported = converse(&mut guarded).await;
        assert_eq!(reported[0], "got=hello", "stdin must reach the child");
        assert_eq!(
            reported[1], "own=ok",
            "the child's own root must be writable: {reported:?}"
        );
        assert_eq!(
            reported[2], "workspace=denied",
            "a root not in the policy must be refused: {reported:?}"
        );
        assert!(!workspace.join("escape").exists());

        let grandchild: i32 = reported[3]
            .strip_prefix("pid=")
            .and_then(|pid| pid.parse().ok())
            .unwrap_or_else(|| panic!("expected `pid=N`, got {reported:?}"));
        assert!(alive(grandchild), "the grandchild should be running");

        drop(guarded);

        // SIGKILL is asynchronous, and the grandchild is reaped by init, so
        // it may take a moment to disappear.
        let deadline = Instant::now() + Duration::from_secs(20);
        while alive(grandchild) {
            assert!(
                Instant::now() < deadline,
                "grandchild {grandchild} survived the dropped guard"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
}
