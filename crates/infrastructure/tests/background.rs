//! Do background jobs run, report and - above all - stop as promised?
//!
//! Real `/bin/sh` jobs under the real sandbox, like `tests/sandbox.rs`. The
//! claims worth making about a job are about processes: that the lifetime cap
//! kills it, that `kill` and dropping the supervisor leave nothing behind
//! (not even a process that called `setsid`), and that the confinement is the
//! one `run_command` has. Waiting is always a bounded poll on process or
//! output state, never a sleep standing in for "long enough".

use std::sync::Arc;
use std::time::{Duration, Instant};

use agent_domain::error::JobError;
use agent_domain::model::dependency::{FileChange, ManifestKind};
use agent_domain::model::workspace::{WorkspacePath, WorkspaceRoot};
use agent_domain::ports::background::{BackgroundCommands, JobId, JobLimits, JobOutput, JobStop};
use agent_domain::ports::command::{CommandRequest, CommandRunner};
use agent_infrastructure::exec::background::BackgroundSupervisor;
use agent_infrastructure::exec::{ExecConfig, SandboxRequirement, SandboxedCommandRunner};

/// Generous: these bound a poll, they are not waited out.
const PATIENCE: Duration = Duration::from_secs(30);

struct Fixture {
    _dir: tempfile::TempDir,
    root: Arc<WorkspaceRoot>,
    jobs: BackgroundSupervisor,
}

fn limits() -> JobLimits {
    JobLimits {
        max_jobs: 4,
        max_lifetime: Duration::from_secs(120),
        buffer_bytes: 64 * 1024,
    }
}

async fn fixture(limits: JobLimits) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let canonical = std::fs::canonicalize(dir.path()).unwrap();
    let root = Arc::new(WorkspaceRoot::new(canonical).unwrap());
    let runner = SandboxedCommandRunner::start(root.clone(), ExecConfig::default())
        .await
        .expect("the dev container must support the sandbox");
    let jobs = BackgroundSupervisor::new(Arc::new(runner), limits).unwrap();
    Fixture {
        _dir: dir,
        root,
        jobs,
    }
}

impl Fixture {
    async fn start(&self, command: &str) -> JobId {
        self.jobs
            .start(CommandRequest {
                command: command.to_string(),
                cwd: WorkspacePath::root(),
                timeout: Duration::from_secs(120),
                max_output_bytes: 64 * 1024,
            })
            .await
            .unwrap()
    }

    fn path(&self, relative: &str) -> std::path::PathBuf {
        self.root.as_path().join(relative)
    }

    /// Reads from `since` until `done` holds for the output, returning the
    /// first output for which it does.
    async fn read_until(
        &self,
        id: JobId,
        since: u64,
        done: impl Fn(&JobOutput) -> bool,
    ) -> JobOutput {
        let deadline = Instant::now() + PATIENCE;
        loop {
            let output = self.jobs.read(id, since, 64 * 1024).await.unwrap();
            if done(&output) {
                return output;
            }
            assert!(Instant::now() < deadline, "gave up waiting: {output:?}");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// The pid a job wrote (atomically, through `mv`) to `relative`.
    async fn pid_in(&self, relative: &str) -> i32 {
        let path = self.path(relative);
        let deadline = Instant::now() + PATIENCE;
        loop {
            if let Ok(text) = std::fs::read_to_string(&path) {
                return text.trim().parse().expect("a pid");
            }
            assert!(Instant::now() < deadline, "`{relative}` never appeared");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
}

/// The session of `pid`, or `None` once it no longer exists - not even as a
/// zombie waiting to be reaped.
fn session_of(pid: i32) -> Option<i32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let (_, rest) = stat.rsplit_once(')')?;
    rest.split_whitespace().nth(3)?.parse().ok()
}

async fn assert_gone(pid: i32, what: &str) {
    let deadline = Instant::now() + PATIENCE;
    while session_of(pid).is_some() {
        assert!(Instant::now() < deadline, "{what} ({pid}) is still there");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn output_is_read_incrementally_and_the_exit_code_is_reported() {
    let fixture = fixture(limits()).await;
    let id = fixture
        .start("echo one; while [ ! -e go ]; do sleep 0.01; done; echo two; exit 3")
        .await;

    let first = fixture
        .read_until(id, 0, |output| output.chunk.contains("one"))
        .await;
    assert!(first.running, "{first:?}");
    assert_eq!(first.exit_code, None);

    std::fs::write(fixture.path("go"), "").unwrap();
    let rest = fixture
        .read_until(id, first.next, |output| !output.running)
        .await;
    assert_eq!(rest.chunk, "two\n", "only what is new: {rest:?}");
    assert_eq!(rest.dropped, 0);
    assert_eq!(rest.exit_code, Some(3));
    assert_eq!(rest.stopped, None);
}

#[tokio::test]
async fn kill_stops_the_job_and_every_descendant_even_one_that_called_setsid() {
    let fixture = fixture(limits()).await;
    let id = fixture
        .start(
            "echo $$ > leader.tmp && mv leader.tmp leader.pid\n\
             sleep 100 & echo $! > child.tmp && mv child.tmp child.pid\n\
             setsid sh -c 'echo $$ > detached.tmp && mv detached.tmp detached.pid; \
             exec sleep 100' >/dev/null 2>&1 &\n\
             wait",
        )
        .await;
    let leader = fixture.pid_in("leader.pid").await;
    let child = fixture.pid_in("child.pid").await;
    let detached = fixture.pid_in("detached.pid").await;
    assert_eq!(
        session_of(detached),
        Some(detached),
        "the probe must really have left the job's session"
    );

    let output = fixture.jobs.kill(id, 1024).await.unwrap();
    assert!(!output.running, "{output:?}");
    assert_eq!(output.stopped, Some(JobStop::Killed));

    for (pid, what) in [
        (leader, "the job"),
        (child, "its child"),
        (detached, "its setsid child"),
    ] {
        assert_gone(pid, what).await;
    }

    // Killing again is harmless, and the job is still readable.
    let again = fixture.jobs.kill(id, 1024).await.unwrap();
    assert!(!again.running);
}

#[tokio::test]
async fn the_lifetime_cap_kills_the_job() {
    let fixture = fixture(JobLimits {
        max_lifetime: Duration::from_secs(1),
        ..limits()
    })
    .await;
    let id = fixture.start("echo started; exec sleep 100").await;

    let output = fixture.read_until(id, 0, |output| !output.running).await;
    assert_eq!(
        output.stopped,
        Some(JobStop::LifetimeExceeded { seconds: 1 }),
        "{output:?}"
    );
    assert_eq!(output.exit_code, None, "killed by a signal");
    assert!(output.chunk.contains("started"), "{output:?}");
}

#[tokio::test]
async fn the_buffer_keeps_the_tail_and_reports_what_it_dropped() {
    let fixture = fixture(JobLimits {
        buffer_bytes: 1024,
        ..limits()
    })
    .await;
    let id = fixture
        .start("i=0; while [ $i -lt 500 ]; do echo line$i; i=$((i+1)); done")
        .await;

    let output = fixture.read_until(id, 0, |output| !output.running).await;
    assert_eq!(output.exit_code, Some(0), "{output:?}");
    assert!(output.chunk.len() <= 1024, "kept {}", output.chunk.len());
    assert!(output.chunk.ends_with("line499\n"), "{:?}", output.chunk);
    assert!(output.dropped > 0, "{output:?}");
    assert_eq!(
        output.dropped + output.chunk.len() as u64,
        output.next,
        "everything before the chunk is accounted for"
    );
}

#[tokio::test]
async fn the_concurrency_cap_refuses_the_next_job_in_words_the_model_can_act_on() {
    let fixture = fixture(JobLimits {
        max_jobs: 1,
        ..limits()
    })
    .await;
    let first = fixture.start("exec sleep 100").await;

    let error = fixture
        .jobs
        .start(CommandRequest {
            command: "true".into(),
            cwd: WorkspacePath::root(),
            timeout: Duration::from_secs(10),
            max_output_bytes: 1024,
        })
        .await
        .unwrap_err();
    assert_eq!(error, JobError::TooMany { limit: 1 });
    assert!(error.to_string().contains("kill_command"), "{error}");

    fixture.jobs.kill(first, 1024).await.unwrap();
    fixture.start("true").await;
}

#[tokio::test]
async fn a_manifest_the_job_edited_is_reported_with_the_read_that_sees_the_end() {
    let fixture = fixture(limits()).await;
    std::fs::write(fixture.path("Cargo.toml"), "[dependencies]\n").unwrap();
    let id = fixture
        .start(
            "while [ ! -e go ]; do sleep 0.01; done\n\
             echo 'evil = \"1\"' >> Cargo.toml",
        )
        .await;

    let running = fixture.jobs.read(id, 0, 1024).await.unwrap();
    assert!(running.running);
    assert_eq!(running.dependencies, None, "nothing to report yet");

    std::fs::write(fixture.path("go"), "").unwrap();
    let ended = fixture.read_until(id, 0, |output| !output.running).await;
    let report = ended
        .dependencies
        .expect("the first read that sees the end carries the report");
    assert!(report.complete);
    assert_eq!(report.changes.len(), 1, "{report:?}");
    assert_eq!(
        report.changes[0].path,
        WorkspacePath::parse("Cargo.toml").unwrap()
    );
    assert_eq!(report.changes[0].kind, ManifestKind::CargoToml);
    assert_eq!(report.changes[0].change, FileChange::Modified);

    let later = fixture.jobs.read(id, 0, 1024).await.unwrap();
    assert_eq!(later.dependencies, None, "reported once, not on every read");
}

#[tokio::test]
async fn dropping_the_supervisor_kills_every_job() {
    let fixture = fixture(limits()).await;
    let mut pids = Vec::new();
    for name in ["a", "b"] {
        fixture
            .start(&format!(
                "echo $$ > {name}.tmp && mv {name}.tmp {name}.pid\n\
                 sleep 100 & echo $! > {name}c.tmp && mv {name}c.tmp {name}c.pid\n\
                 wait"
            ))
            .await;
        pids.push(fixture.pid_in(&format!("{name}.pid")).await);
        pids.push(fixture.pid_in(&format!("{name}c.pid")).await);
    }
    let Fixture {
        _dir,
        root: _,
        jobs,
    } = fixture;

    drop(jobs);

    for pid in pids {
        assert_gone(pid, "a job or its child").await;
    }
}

#[tokio::test]
async fn a_job_cannot_write_outside_the_workspace() {
    let outside = tempfile::tempdir().unwrap();
    let dir = std::fs::canonicalize(outside.path()).unwrap();
    let fixture = fixture(limits()).await;

    // Control: unconfined, the very same write succeeds, so a failure below is
    // the kernel refusing rather than the probe being broken.
    let control = dir.join("control.txt");
    let unconfined = SandboxedCommandRunner::start(
        fixture.root.clone(),
        ExecConfig {
            sandbox: SandboxRequirement::Disabled,
            ..ExecConfig::default()
        },
    )
    .await
    .unwrap();
    let baseline = unconfined
        .run(CommandRequest {
            command: format!("echo ok > {}", control.display()),
            cwd: WorkspacePath::root(),
            timeout: Duration::from_secs(30),
            max_output_bytes: 1024,
        })
        .await
        .unwrap();
    assert!(baseline.succeeded() && control.exists(), "{baseline:?}");

    let target = dir.join("escaped.txt");
    let id = fixture
        .start(&format!("echo pwned > {}", target.display()))
        .await;
    let output = fixture.read_until(id, 0, |output| !output.running).await;

    assert_ne!(output.exit_code, Some(0), "{output:?}");
    assert!(!target.exists(), "the job wrote outside the workspace");
}

#[tokio::test]
async fn background_jobs_are_refused_without_a_confining_sandbox() {
    let dir = tempfile::tempdir().unwrap();
    let root = Arc::new(WorkspaceRoot::new(std::fs::canonicalize(dir.path()).unwrap()).unwrap());
    let unconfined = SandboxedCommandRunner::start(
        root,
        ExecConfig {
            sandbox: SandboxRequirement::Disabled,
            ..ExecConfig::default()
        },
    )
    .await
    .unwrap();
    assert!(BackgroundSupervisor::new(Arc::new(unconfined), limits()).is_err());
}
