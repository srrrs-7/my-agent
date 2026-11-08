//! What `protect_agent_process` does to this process, proved with a real
//! child.
//!
//! A test binary of its own, because the setting is process-wide and cannot
//! be undone: every other test in a shared binary would run against a
//! non-dumpable process from then on, whichever order they happened to run in.
//!
//! Linux only: elsewhere the call does nothing, and there is no `/proc`.

#![cfg(target_os = "linux")]

use std::sync::Arc;
use std::time::Duration;

use agent_domain::model::workspace::{WorkspacePath, WorkspaceRoot};
use agent_domain::ports::command::{CommandRequest, CommandRunner};
use agent_infrastructure::exec::{
    ExecConfig, SandboxRequirement, SandboxedCommandRunner, protect_agent_process,
};

fn request(command: &str) -> CommandRequest {
    CommandRequest {
        command: command.to_string(),
        cwd: WorkspacePath::root(),
        timeout: Duration::from_secs(30),
        max_output_bytes: 16 * 1024,
    }
}

/// The case Landlock does not cover: `AGENT_SHELL_SANDBOX=none`. An
/// unconfined child of the same user reads the agent's environment - keys
/// and all - until the agent clears its dumpable flag.
#[tokio::test]
async fn an_unconfined_child_cannot_read_the_environment_of_a_protected_agent() {
    let dir = tempfile::tempdir().unwrap();
    let root = Arc::new(WorkspaceRoot::new(std::fs::canonicalize(dir.path()).unwrap()).unwrap());
    let runner = SandboxedCommandRunner::start(
        root,
        ExecConfig {
            sandbox: SandboxRequirement::Disabled,
            ..ExecConfig::default()
        },
    )
    .await
    .unwrap();
    let probe = format!(
        "if cat /proc/{}/environ >/dev/null 2>&1; then echo environ=read; \
         else echo environ=denied; fi",
        std::process::id()
    );

    // Control: the same child, the same pid, before the call.
    let before = runner.run(request(&probe)).await.unwrap();
    assert_eq!(before.stdout.trim(), "environ=read", "{before:?}");

    protect_agent_process().expect("PR_SET_DUMPABLE can be cleared");

    let after = runner.run(request(&probe)).await.unwrap();
    assert_eq!(
        after.stdout.trim(),
        "environ=denied",
        "a non-dumpable agent's environment must be closed to its children: {after:?}"
    );

    // The stray sweep reads `/proc` too. It must keep working on a process
    // whose own `/proc` entries now belong to root.
    let detached = runner
        .run(request(
            "setsid sh -c 'echo $$ > p.tmp && mv p.tmp p; exec sleep 30' >/dev/null 2>&1 &\n\
             while [ ! -s p ]; do sleep 0.01; done\n\
             cat p",
        ))
        .await
        .unwrap();
    let pid: i32 = detached.stdout.trim().parse().expect("a pid");
    assert!(
        std::fs::metadata(format!("/proc/{pid}")).is_err(),
        "detached process {pid} survived the command on a protected agent"
    );
}
