//! Does the sandbox actually confine a real child process?
//!
//! Everything here spawns a genuine `/bin/sh` and checks what it managed to
//! do. Unit tests can prove a ruleset *builds*; only this can prove the kernel
//! *enforced* it, which is the only claim worth making about a sandbox.
//!
//! Linux-only for now: on other platforms `SandboxedCommandRunner::new` fails
//! closed, which is itself covered below.

use std::sync::Arc;
use std::time::Duration;

use agent_domain::error::CommandError;
use agent_domain::model::workspace::{WorkspacePath, WorkspaceRoot};
use agent_domain::ports::command::{CommandRequest, CommandRunner, SandboxKind};
use agent_infrastructure::exec::{
    ExecConfig, SandboxRequirement, SandboxedCommandRunner, detect_sandbox,
};

struct Fixture {
    _dir: tempfile::TempDir,
    root: Arc<WorkspaceRoot>,
    runner: SandboxedCommandRunner,
}

async fn fixture(config: ExecConfig) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    // The sandbox pins paths by their real location, and macOS/CI temp dirs
    // are often symlinks.
    let canonical = std::fs::canonicalize(dir.path()).unwrap();
    let root = Arc::new(WorkspaceRoot::new(canonical).unwrap());
    let runner = SandboxedCommandRunner::start(root.clone(), config)
        .await
        .expect("the dev container must support the sandbox");
    Fixture {
        _dir: dir,
        root,
        runner,
    }
}

/// The same workspace with the sandbox explicitly off.
///
/// Every restriction test below pairs with one of these. A test that only
/// asserts "the command failed" proves nothing on its own - it passes just as
/// happily when the probe is broken, which is exactly how an earlier version
/// of this file reported a blocked connection that the shell had never been
/// able to attempt.
async fn unsandboxed(root: Arc<WorkspaceRoot>) -> SandboxedCommandRunner {
    SandboxedCommandRunner::start(
        root,
        ExecConfig {
            sandbox: SandboxRequirement::Disabled,
            ..ExecConfig::default()
        },
    )
    .await
    .unwrap()
}

fn request(command: &str) -> CommandRequest {
    CommandRequest {
        command: command.to_string(),
        cwd: WorkspacePath::root(),
        timeout: Duration::from_secs(30),
        max_output_bytes: 16 * 1024,
    }
}

#[tokio::test]
async fn the_dev_container_reports_landlock() {
    let available = detect_sandbox();
    assert_eq!(
        available.kind,
        SandboxKind::LandlockConfined,
        "shortfall: {:?}",
        available.shortfall
    );
}

#[tokio::test]
async fn a_command_runs_and_its_output_comes_back() {
    let fixture = fixture(ExecConfig::default()).await;
    let output = fixture
        .runner
        .run(request("echo hello; echo oops >&2; exit 7"))
        .await
        .unwrap();

    assert_eq!(output.exit_code, Some(7));
    assert_eq!(output.stdout.trim(), "hello");
    assert_eq!(output.stderr.trim(), "oops");
}

#[tokio::test]
async fn writes_inside_the_workspace_are_allowed() {
    let fixture = fixture(ExecConfig::default()).await;
    let output = fixture
        .runner
        .run(request("mkdir -p sub && echo written > sub/file.txt"))
        .await
        .unwrap();

    assert!(output.succeeded(), "stderr: {}", output.stderr);
    let written = std::fs::read_to_string(fixture.root.as_path().join("sub/file.txt")).unwrap();
    assert_eq!(written.trim(), "written");
}

#[tokio::test]
async fn writes_outside_the_workspace_are_refused_by_the_kernel() {
    let outside = tempfile::tempdir().unwrap();
    let dir = std::fs::canonicalize(outside.path()).unwrap();

    let fixture = fixture(ExecConfig::default()).await;

    // Control: without the sandbox the very same write succeeds, so a failure
    // below is the kernel refusing rather than the probe being broken.
    let control = dir.join("control.txt");
    let baseline = unsandboxed(fixture.root.clone())
        .await
        .run(request(&format!("echo ok > {}", control.display())))
        .await
        .unwrap();
    assert!(
        baseline.succeeded() && control.exists(),
        "the control write must succeed: {baseline:?}"
    );

    let target = dir.join("escaped.txt");
    let output = fixture
        .runner
        .run(request(&format!("echo pwned > {}", target.display())))
        .await
        .unwrap();

    assert!(
        !output.succeeded(),
        "the write must fail, got stdout={:?} stderr={:?}",
        output.stdout,
        output.stderr
    );
    assert!(
        !target.exists(),
        "nothing may be created outside the workspace"
    );
}

#[tokio::test]
async fn the_shared_tmp_is_not_writable_but_the_session_scratch_is() {
    let fixture = fixture(ExecConfig::default()).await;

    // Toolchains need scratch space, so TMPDIR must work...
    let scratch = fixture
        .runner
        .run(request(
            "echo scratch > \"$TMPDIR/probe\" && cat \"$TMPDIR/probe\"",
        ))
        .await
        .unwrap();
    assert!(scratch.succeeded(), "stderr: {}", scratch.stderr);
    assert_eq!(scratch.stdout.trim(), "scratch");

    // ...but that must not amount to handing over all of /tmp.
    let shared = fixture
        .runner
        .run(request("echo pwned > /tmp/my-agent-escape-probe"))
        .await
        .unwrap();
    assert!(
        !shared.succeeded(),
        "the shared /tmp must stay read-only: {shared:?}"
    );
}

#[tokio::test]
async fn the_usual_pseudo_devices_stay_usable() {
    // `>/dev/null` is in almost every command line. A sandbox that breaks it
    // is unusable regardless of how well it confines anything else.
    let fixture = fixture(ExecConfig::default()).await;

    let output = fixture
        .runner
        .run(request(
            "echo quiet >/dev/null && head -c 8 /dev/urandom >/dev/null && echo OK",
        ))
        .await
        .unwrap();
    assert!(output.succeeded(), "stderr: {}", output.stderr);
    assert_eq!(output.stdout.trim(), "OK");

    // The grant is per-node, not all of `/dev`: creating a new entry there
    // must still fail.
    let created = fixture
        .runner
        .run(request("echo x > /dev/my-agent-probe"))
        .await
        .unwrap();
    assert!(
        !created.succeeded(),
        "/dev must not be writable: {created:?}"
    );
}

/// A command must not reach past the pipes we capture and touch the user's
/// terminal - neither to print an imitation of the approval prompt nor to read
/// what the user types next. Its own session is what makes `/dev/tty`
/// unopenable, because a session with no controlling terminal has nothing for
/// that device to refer to.
///
/// This asserts the *mechanism*, not the effect. The dev container has no
/// controlling terminal at all, so `echo > /dev/tty` fails there whether or not
/// we do anything - a test written that way would pass while proving nothing.
/// Session leadership holds in every environment.
#[tokio::test]
async fn the_child_leads_its_own_session() {
    let fixture = fixture(ExecConfig::default()).await;
    let output = fixture
        .runner
        .run(request("ps -o sid=,pid= -p $$"))
        .await
        .unwrap();

    let numbers: Vec<&str> = output.stdout.split_whitespace().collect();
    assert_eq!(numbers.len(), 2, "expected `sid pid`, got {output:?}");
    assert_eq!(
        numbers[0], numbers[1],
        "the shell must be its own session leader, so it inherits no terminal: {output:?}"
    );
}

#[tokio::test]
async fn reading_outside_the_workspace_still_works() {
    // Reads are deliberately broad: a read allowlist tight enough to matter
    // breaks every toolchain. Credentials are defended by env scrubbing and
    // the egress allowlist instead, and this pins that documented choice.
    let fixture = fixture(ExecConfig::default()).await;
    let output = fixture
        .runner
        .run(request("cat /etc/hostname"))
        .await
        .unwrap();
    assert!(output.succeeded(), "stderr: {}", output.stderr);
}

/// Pins a limitation rather than a guarantee.
///
/// `.git` sits inside the writable workspace, and Landlock has no way to carve
/// it back out: its rules are a union of allow-lists with no deny and no
/// most-specific-wins precedence. A command can therefore rewrite history or
/// plant a hook on this tier. Protecting it needs a mechanism that can express
/// denial - a bind mount, or Seatbelt's `deny` - so it belongs to the stronger
/// tiers. If this test ever starts failing, the kernel gained a precedence
/// rule and the docs claiming otherwise need revisiting.
#[tokio::test]
async fn landlock_cannot_protect_git_inside_the_writable_workspace() {
    let fixture = fixture(ExecConfig::default()).await;
    std::fs::create_dir_all(fixture.root.as_path().join(".git")).unwrap();
    std::fs::write(fixture.root.as_path().join(".git/config"), "[core]\n").unwrap();

    let output = fixture
        .runner
        .run(request("echo '[remote \"evil\"]' >> .git/config"))
        .await
        .unwrap();

    assert!(
        output.succeeded(),
        "documented limitation: Landlock cannot deny a subtree of a writable \
         root, so this write is expected to succeed. stderr={:?}",
        output.stderr
    );
}

#[tokio::test]
async fn an_extra_writable_root_is_honoured() {
    // The dev container puts CARGO_TARGET_DIR outside the workspace, so this
    // escape hatch is what keeps `cargo test` runnable at all.
    let cache = tempfile::tempdir().unwrap();
    let cache_path = std::fs::canonicalize(cache.path()).unwrap();

    let fixture = fixture(ExecConfig {
        extra_writable: vec![cache_path.clone()],
        ..ExecConfig::default()
    })
    .await;
    let output = fixture
        .runner
        .run(request(&format!(
            "echo cached > {}/artifact",
            cache_path.display()
        )))
        .await
        .unwrap();

    assert!(output.succeeded(), "stderr: {}", output.stderr);
    assert!(cache_path.join("artifact").exists());
}

/// A listener that accepts and immediately drops, plus its port.
async fn accepting_port() -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move { while listener.accept().await.is_ok() {} });
    port
}

/// `/bin/sh` here is dash, which has no `/dev/tcp`, so the connection attempt
/// goes through curl. Its exit code alone tells us whether the kernel let the
/// socket open.
///
/// `--noproxy '*'` matters: the runner exports `HTTP_PROXY`, and without this
/// curl would dial the proxy instead of the port under test - measuring the
/// environment plumbing rather than the kernel rule.
fn connect_probe(port: u16) -> String {
    format!(
        "curl --noproxy '*' --silent --max-time 3 http://127.0.0.1:{port}/ >/dev/null 2>&1; \
         echo rc=$?"
    )
}

/// curl exits 7 ("failed to connect") when the kernel refuses the socket. The
/// listeners here accept and drop, so a permitted connection shows up as 52 or
/// 56 instead - anything but 7 means a socket was opened.
fn connected(stdout: &str) -> bool {
    !stdout.contains("rc=7")
}

#[tokio::test]
async fn outbound_tcp_is_blocked_when_no_proxy_is_configured() {
    let port = accepting_port().await;
    let fixture = fixture(ExecConfig::default()).await;

    // Control: the probe must be able to report success, or the assertion
    // below would pass even with the sandbox disabled.
    let baseline = unsandboxed(fixture.root.clone())
        .await
        .run(request(&connect_probe(port)))
        .await
        .unwrap();
    assert!(
        connected(&baseline.stdout),
        "the probe cannot detect a connection at all: {baseline:?}"
    );

    let output = fixture
        .runner
        .run(request(&connect_probe(port)))
        .await
        .unwrap();
    assert!(
        !connected(&output.stdout),
        "the sandbox must block outbound TCP: {output:?}"
    );
}

#[tokio::test]
async fn only_the_egress_proxy_port_is_connectable() {
    let other_port = accepting_port().await;

    // A non-empty allowlist is what starts the proxy. Which domains are on it
    // does not matter here - only that the kernel rule names the port it bound.
    let fixture = fixture(ExecConfig {
        allowed_domains: vec!["crates.io".to_string()],
        ..ExecConfig::default()
    })
    .await;
    let proxy_port = fixture
        .runner
        .egress_port()
        .expect("a non-empty allowlist must start the proxy");

    let allowed = fixture
        .runner
        .run(request(&connect_probe(proxy_port)))
        .await
        .unwrap();
    assert!(
        connected(&allowed.stdout),
        "the proxy port must stay reachable: {allowed:?}"
    );

    let blocked = fixture
        .runner
        .run(request(&connect_probe(other_port)))
        .await
        .unwrap();
    assert!(
        !connected(&blocked.stdout),
        "every other port must be refused: {blocked:?}"
    );
}

/// The two halves of the allowlist are configured in different places - the
/// proxy decides by name, the kernel decides by port - so this walks the path
/// a real command takes and checks they line up.
#[tokio::test]
async fn an_unlisted_domain_is_refused_by_the_proxy_the_child_is_pinned_to() {
    let fixture = fixture(ExecConfig {
        allowed_domains: vec!["crates.io".to_string()],
        ..ExecConfig::default()
    })
    .await;

    // No `--noproxy` this time: curl follows the exported `HTTP_PROXY`, which
    // is the only route out of the sandbox.
    let output = fixture
        .runner
        .run(request(
            "curl --silent --show-error --max-time 5 http://not-on-the-list.example/ 2>&1",
        ))
        .await
        .unwrap();

    assert!(
        output.stdout.contains("egress proxy refused"),
        "the refusal must reach the caller as a policy message: {output:?}"
    );
}

#[tokio::test]
async fn no_allowlist_means_no_proxy_and_no_proxy_variables() {
    let fixture = fixture(ExecConfig::default()).await;
    assert_eq!(
        fixture.runner.egress_port(),
        None,
        "an empty allowlist must not bind a port"
    );

    let output = fixture
        .runner
        .run(request(
            "echo \"proxy=[${HTTP_PROXY-unset}] noproxy=[${NO_PROXY-unset}]\"",
        ))
        .await
        .unwrap();
    assert!(
        output.stdout.contains("proxy=[unset]") && output.stdout.contains("noproxy=[unset]"),
        "a child with no network must not inherit proxy settings: {output:?}"
    );
}

#[tokio::test]
async fn secrets_are_stripped_from_the_child_environment() {
    // SAFETY: single-threaded setup before any child is spawned, and the
    // variables are unique to this test.
    unsafe {
        std::env::set_var("SANDBOXTEST_API_KEY", "super-secret");
        std::env::set_var("SANDBOXTEST_PLAIN", "harmless");
    }

    let fixture = fixture(ExecConfig::default()).await;
    let output = fixture
        .runner
        .run(request(
            "echo \"key=[${SANDBOXTEST_API_KEY-unset}] plain=[${SANDBOXTEST_PLAIN-unset}]\"",
        ))
        .await
        .unwrap();

    assert!(
        output.stdout.contains("key=[unset]"),
        "the API key must not reach the child: {}",
        output.stdout
    );
    assert!(
        output.stdout.contains("plain=[harmless]"),
        "ordinary variables must survive: {}",
        output.stdout
    );

    unsafe {
        std::env::remove_var("SANDBOXTEST_API_KEY");
        std::env::remove_var("SANDBOXTEST_PLAIN");
    }
}

#[tokio::test]
async fn a_runaway_command_is_killed_with_its_children() {
    let fixture = fixture(ExecConfig::default()).await;
    let error = fixture
        .runner
        .run(CommandRequest {
            command: "sleep 60 & wait".to_string(),
            cwd: WorkspacePath::root(),
            timeout: Duration::from_millis(300),
            max_output_bytes: 4096,
        })
        .await
        .unwrap_err();

    assert!(matches!(error, CommandError::TimedOut { .. }), "{error:?}");
}

#[tokio::test]
async fn a_cwd_outside_the_workspace_cannot_be_requested() {
    let fixture = fixture(ExecConfig::default()).await;
    let error = fixture
        .runner
        .run(CommandRequest {
            command: "pwd".to_string(),
            // `WorkspacePath` cannot represent an escape, so the only way to
            // aim outside is a path that does not exist inside.
            cwd: WorkspacePath::parse("no/such/dir").unwrap(),
            timeout: Duration::from_secs(5),
            max_output_bytes: 4096,
        })
        .await
        .unwrap_err();

    assert!(matches!(error, CommandError::Refused { .. }), "{error:?}");
}

#[tokio::test]
async fn requiring_more_than_the_platform_offers_fails_closed() {
    let dir = tempfile::tempdir().unwrap();
    let root = Arc::new(WorkspaceRoot::new(std::fs::canonicalize(dir.path()).unwrap()).unwrap());

    let outcome = SandboxedCommandRunner::start(
        root,
        ExecConfig {
            // Stronger than Landlock; not implemented on any platform yet.
            sandbox: SandboxRequirement::Isolated,
            ..ExecConfig::default()
        },
    )
    .await;

    match outcome {
        Err(error) => assert!(
            matches!(error, CommandError::SandboxUnavailable { .. }),
            "{error:?}"
        ),
        Ok(_) => panic!("must refuse to run with less confinement than required"),
    }
}
