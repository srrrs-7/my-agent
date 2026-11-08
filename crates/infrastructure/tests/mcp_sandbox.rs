//! Does an MCP server actually run confined?
//!
//! Like `sandbox.rs`, everything here starts a real process and checks what it
//! managed to do. The server is a few lines of `/bin/sh` that speaks just
//! enough of the protocol to be told "write here", "connect there", "start a
//! grandchild" - so the claims below are about the kernel and the process
//! group, not about a fake.
//!
//! Linux only: elsewhere there is no sandbox, and starting a server fails.

#![cfg(target_os = "linux")]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use agent_domain::error::ToolServerError;
use agent_domain::ports::command::SandboxKind;
use agent_domain::ports::tool_server::{RemoteContent, ToolServer};
use agent_infrastructure::exec::{SandboxRequirement, detect_sandbox};
use agent_infrastructure::mcp::{
    McpConfig, McpServer, McpStartError, ServerConfig, WorkspaceAccess, start_servers,
};
use serde_json::{Value, json};

/// A tiny MCP server. Requests arrive one per line; the id is the first
/// `"id":N` on the line, which is the top-level one whichever order the keys
/// are serialized in, because `params` always comes after it.
const SERVER: &str = r#"
reply() { printf '{"jsonrpc":"2.0","id":%s,"result":%s}\n' "$1" "$2"; }
text() { reply "$1" "{\"content\":[{\"type\":\"text\",\"text\":\"$2\"}]}"; }
arg() { printf '%s\n' "$line" | sed -n "s/.*\"$1\":\"\{0,1\}\([^\",}]*\).*/\1/p"; }

while IFS= read -r line; do
  id=$(printf '%s\n' "$line" | grep -o '"id":[0-9]*' | head -n 1 | cut -d: -f2)
  [ -z "$id" ] && continue
  case "$line" in
    *'"method":"initialize"'*)
      reply "$id" '{"protocolVersion":"2025-06-18","capabilities":{"tools":{}},"serverInfo":{"name":"probe","version":"0"}}' ;;
    *'"method":"tools/list"'*)
      case "$line" in
        *'"cursor":"p2"'*)
          reply "$id" '{"tools":[{"name":"spawn","inputSchema":{"type":"object"}},{"name":"crash"},{"name":"env","inputSchema":{"type":"object"}},{"name":"detach"},{"name":"peek"}]}' ;;
        *)
          reply "$id" '{"tools":[{"name":"write","description":"Writes.","inputSchema":{"type":"object"},"annotations":{"readOnlyHint":true}},{"name":"connect"},{"name":"fetch"}],"nextCursor":"p2"}' ;;
      esac ;;
    *'"method":"tools/call"'*)
      case "$line" in
        *'"name":"write"'*)
          if printf x > "$(arg path)" 2>/dev/null; then text "$id" ok; else text "$id" denied; fi ;;
        *'"name":"connect"'*)
          curl --noproxy '*' --silent --max-time 3 "http://127.0.0.1:$(arg port)/" >/dev/null 2>&1
          text "$id" "rc=$?" ;;
        *'"name":"fetch"'*)
          if curl --silent --show-error --max-time 5 "http://$(arg host)/" 2>&1 | grep -q 'egress proxy refused'
          then text "$id" refused; else text "$id" not-refused; fi ;;
        *'"name":"spawn"'*)
          sleep 100 >/dev/null 2>&1 &
          text "$id" "pid=$!" ;;
        *'"name":"detach"'*)
          setsid sleep 100 >/dev/null 2>&1 &
          text "$id" "pid=$!" ;;
        *'"name":"peek"'*)
          if cat "/proc/$(arg pid)/environ" >/dev/null 2>&1; then text "$id" read; else text "$id" denied; fi ;;
        *'"name":"env"'*)
          text "$id" "value=$(printenv "$(arg var)")" ;;
        *'"name":"crash"'*)
          exit 1 ;;
      esac ;;
  esac
done
"#;

struct Fixture {
    _dirs: Vec<tempfile::TempDir>,
    workspace: PathBuf,
    script: PathBuf,
}

/// A workspace, and the server script in a separate directory outside it.
fn fixture() -> Fixture {
    let workspace_dir = tempfile::tempdir().unwrap();
    let scripts = tempfile::tempdir().unwrap();
    let workspace = std::fs::canonicalize(workspace_dir.path()).unwrap();
    let script = std::fs::canonicalize(scripts.path())
        .unwrap()
        .join("server.sh");
    std::fs::write(&script, SERVER).unwrap();
    Fixture {
        _dirs: vec![workspace_dir, scripts],
        workspace,
        script,
    }
}

fn config(script: &Path) -> ServerConfig {
    ServerConfig {
        command: PathBuf::from("/bin/sh"),
        args: vec![script.display().to_string()],
        env: BTreeMap::new(),
        env_from: Vec::new(),
        inherited: BTreeMap::new(),
        allow_hosts: Vec::new(),
        workspace_access: WorkspaceAccess::None,
    }
}

fn landlock() -> SandboxKind {
    // The dev container is the reference environment, so missing Landlock is
    // a loud failure rather than a silent skip - as in `sandbox.rs`.
    let available = detect_sandbox();
    assert_eq!(
        available.kind,
        SandboxKind::LandlockConfined,
        "shortfall: {:?}",
        available.shortfall
    );
    available.kind
}

async fn start(fixture: &Fixture, config: &ServerConfig) -> McpServer {
    McpServer::start("probe", config, landlock(), &fixture.workspace, &[])
        .await
        .expect("the probe server starts")
}

async fn call(server: &McpServer, tool: &str, arguments: Value) -> String {
    let output = tokio::time::timeout(Duration::from_secs(30), server.call(tool, arguments))
        .await
        .expect("the server stopped answering")
        .unwrap_or_else(|error| panic!("`{tool}` failed: {error}"));
    assert!(!output.is_error, "{output:?}");
    match output.content.as_slice() {
        [RemoteContent::Text(text)] => text.clone(),
        other => panic!("expected one text item, got {other:?}"),
    }
}

#[tokio::test]
async fn the_handshake_lists_every_page_of_tools() {
    let fixture = fixture();
    let server = start(&fixture, &config(&fixture.script)).await;

    let names: Vec<String> = server.tools().into_iter().map(|tool| tool.name).collect();
    assert_eq!(
        names,
        [
            "write", "connect", "fetch", "spawn", "crash", "env", "detach", "peek"
        ],
        "both pages, in order"
    );
    assert_eq!(server.sandbox(), SandboxKind::LandlockConfined);
}

#[tokio::test]
async fn by_default_a_server_writes_its_own_directory_and_not_the_workspace() {
    let fixture = fixture();
    let server = start(&fixture, &config(&fixture.script)).await;

    // Control: the same probe succeeds where it is allowed, so `denied` below
    // is the kernel and not a broken probe.
    let own = server.directory().join("probe");
    assert_eq!(
        call(&server, "write", json!({"path": own.display().to_string()})).await,
        "ok"
    );
    assert!(own.exists());

    use std::os::unix::fs::PermissionsExt as _;
    let mode = std::fs::metadata(server.directory())
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o700, "owner-only, got {mode:o}");

    let escape = fixture.workspace.join("Cargo.toml");
    assert_eq!(
        call(
            &server,
            "write",
            json!({"path": escape.display().to_string()})
        )
        .await,
        "denied",
        "the workspace must not be writable without `read-write`"
    );
    assert!(!escape.exists());
}

#[tokio::test]
async fn read_write_access_is_what_opens_the_workspace() {
    let fixture = fixture();
    let server = start(
        &fixture,
        &ServerConfig {
            workspace_access: WorkspaceAccess::ReadWrite,
            ..config(&fixture.script)
        },
    )
    .await;

    let target = fixture.workspace.join("written");
    assert_eq!(
        call(
            &server,
            "write",
            json!({"path": target.display().to_string()})
        )
        .await,
        "ok"
    );
    assert!(target.exists());

    // Still nowhere else.
    let elsewhere = tempfile::tempdir().unwrap();
    let outside = std::fs::canonicalize(elsewhere.path()).unwrap().join("x");
    assert_eq!(
        call(
            &server,
            "write",
            json!({"path": outside.display().to_string()})
        )
        .await,
        "denied"
    );
}

/// A listener that accepts and immediately drops, plus its port.
async fn accepting_port() -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move { while listener.accept().await.is_ok() {} });
    port
}

/// curl exits 7 when the kernel refuses the socket; the listeners here accept
/// and drop, so a connection that was allowed shows up as something else.
fn connected(report: &str) -> bool {
    report != "rc=7"
}

/// The same probe the server runs, unconfined: proves it can detect a
/// connection at all.
async fn unconfined_probe(port: u16) -> String {
    let status = tokio::process::Command::new("curl")
        .args(["--noproxy", "*", "--silent", "--max-time", "3"])
        .arg(format!("http://127.0.0.1:{port}/"))
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await
        .unwrap();
    format!("rc={}", status.code().unwrap_or(-1))
}

#[tokio::test]
async fn a_server_without_allow_hosts_has_no_network() {
    let port = accepting_port().await;
    assert!(
        connected(&unconfined_probe(port).await),
        "the probe cannot detect a connection at all"
    );

    let fixture = fixture();
    let server = start(&fixture, &config(&fixture.script)).await;
    assert_eq!(server.egress_port(), None, "no allowlist, no proxy");

    let report = call(&server, "connect", json!({"port": port})).await;
    assert!(
        !connected(&report),
        "outbound TCP must be refused: {report}"
    );
}

#[tokio::test]
async fn a_server_reaches_only_its_own_proxy_which_refuses_unlisted_hosts() {
    let other_port = accepting_port().await;
    assert!(connected(&unconfined_probe(other_port).await));

    let fixture = fixture();
    let server = start(
        &fixture,
        &ServerConfig {
            allow_hosts: vec!["crates.io".to_string()],
            ..config(&fixture.script)
        },
    )
    .await;
    let proxy_port = server
        .egress_port()
        .expect("a non-empty allowlist starts a proxy");

    let report = call(&server, "connect", json!({"port": proxy_port})).await;
    assert!(connected(&report), "the proxy must be reachable: {report}");

    let report = call(&server, "connect", json!({"port": other_port})).await;
    assert!(
        !connected(&report),
        "every other port must be refused: {report}"
    );

    // Through the proxy the server's environment points at - the only way out.
    assert_eq!(
        call(&server, "fetch", json!({"host": "not-on-the-list.example"})).await,
        "refused"
    );
}

#[tokio::test]
async fn the_operators_env_reaches_the_server() {
    let fixture = fixture();
    let server = start(
        &fixture,
        &ServerConfig {
            env: BTreeMap::from([("PROBE_TOKEN".to_string(), "handed-over".to_string())]),
            ..config(&fixture.script)
        },
    )
    .await;

    // A `_TOKEN` name is exactly what scrubbing removes from inherited
    // variables; set explicitly, it must survive.
    assert_eq!(
        call(&server, "env", json!({"var": "PROBE_TOKEN"})).await,
        "value=handed-over"
    );
}

#[tokio::test]
async fn an_inherited_secret_reaches_its_server_though_the_scrub_withholds_it() {
    let fixture = fixture();
    let secret = "inherited-0123456789".to_string();
    // As in production: the value is withheld from every child by value, and
    // handed to this one explicitly.
    let server = McpServer::start(
        "probe",
        &ServerConfig {
            env_from: vec!["PROBE_KEY".to_string()],
            inherited: BTreeMap::from([("PROBE_KEY".to_string(), secret.clone())]),
            ..config(&fixture.script)
        },
        landlock(),
        &fixture.workspace,
        std::slice::from_ref(&secret),
    )
    .await
    .expect("the probe server starts");

    assert_eq!(
        call(&server, "env", json!({"var": "PROBE_KEY"})).await,
        format!("value={secret}")
    );
}

fn alive(pid: i32) -> bool {
    // Signal 0 checks for existence without delivering anything.
    unsafe { libc::kill(pid, 0) == 0 }
}

#[tokio::test]
async fn dropping_the_server_takes_its_grandchildren_with_it() {
    let fixture = fixture();
    let server = start(&fixture, &config(&fixture.script)).await;
    let directory = server.directory().to_path_buf();

    let report = call(&server, "spawn", json!({})).await;
    let grandchild: i32 = report
        .strip_prefix("pid=")
        .and_then(|pid| pid.parse().ok())
        .unwrap_or_else(|| panic!("expected `pid=N`, got {report}"));
    assert!(alive(grandchild), "the grandchild should be running");

    drop(server);

    // SIGKILL is asynchronous, and the grandchild is reaped by init, so it may
    // take a moment to disappear. A bounded poll on process state.
    let deadline = Instant::now() + Duration::from_secs(20);
    while alive(grandchild) {
        assert!(
            Instant::now() < deadline,
            "grandchild {grandchild} survived the dropped server"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(!directory.exists(), "the server's directory is removed");
}

/// The session in `/proc/<pid>/stat`, or `None` once the process has been
/// reaped. A zombie still has one: it is not gone yet.
fn session_of(pid: i32) -> Option<i32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let (_, rest) = stat.rsplit_once(')')?;
    rest.split_whitespace().nth(3)?.parse().ok()
}

/// `setsid` takes a grandchild out of the server's process group, where the
/// group kill cannot reach it. Stopping the server must stop it anyway.
#[tokio::test]
async fn a_grandchild_that_calls_setsid_dies_with_the_server() {
    let fixture = fixture();
    let server = start(&fixture, &config(&fixture.script)).await;

    let report = call(&server, "detach", json!({})).await;
    let detached: i32 = report
        .strip_prefix("pid=")
        .and_then(|pid| pid.parse().ok())
        .unwrap_or_else(|| panic!("expected `pid=N`, got {report}"));

    // `setsid` runs in the forked child before it execs `sleep`, so the
    // session may still be the server's for a moment.
    let deadline = Instant::now() + Duration::from_secs(20);
    while session_of(detached) != Some(detached) {
        assert!(
            Instant::now() < deadline,
            "the probe never left the server's session: {:?}",
            session_of(detached)
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    drop(server);

    // The kill is synchronous in the drop; being reaped is not.
    let deadline = Instant::now() + Duration::from_secs(20);
    while session_of(detached).is_some() {
        assert!(
            Instant::now() < deadline,
            "detached grandchild {detached} survived the dropped server"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// A confined server cannot read the agent's environment, where the provider
/// keys and every other server's `env_from` values are. Landlock's ptrace
/// scoping refuses it; the unconfined control shows the same probe works.
#[tokio::test]
async fn a_confined_server_cannot_read_the_agents_environment() {
    let fixture = fixture();
    let agent = std::process::id();

    let control = McpServer::start(
        "probe",
        &config(&fixture.script),
        SandboxKind::None,
        &fixture.workspace,
        &[],
    )
    .await
    .expect("the unconfined control starts");
    assert_eq!(
        call(&control, "peek", json!({"pid": agent})).await,
        "read",
        "unconfined and dumpable, the agent's environment is readable"
    );
    drop(control);

    let server = start(&fixture, &config(&fixture.script)).await;
    assert_eq!(
        call(&server, "peek", json!({"pid": agent})).await,
        "denied",
        "Landlock must keep a confined server out of the agent's environment"
    );
}

#[tokio::test]
async fn a_crashed_server_fails_every_later_call() {
    let fixture = fixture();
    let server = start(&fixture, &config(&fixture.script)).await;

    let first = server.call("crash", json!({})).await.unwrap_err();
    assert!(
        matches!(first, ToolServerError::Stopped { .. }),
        "{first:?}"
    );
    let second = server
        .call("env", json!({"var": "HOME"}))
        .await
        .unwrap_err();
    assert!(
        matches!(second, ToolServerError::Stopped { .. }),
        "{second:?}"
    );
}

async fn start_failure(fixture: &Fixture, script: &str) -> McpStartError {
    let path = fixture.script.with_file_name("broken.sh");
    std::fs::write(&path, script).unwrap();
    match McpServer::start(
        "broken",
        &config(&path),
        landlock(),
        &fixture.workspace,
        &[],
    )
    .await
    {
        Ok(_) => panic!("the server must not start"),
        Err(error) => error,
    }
}

#[tokio::test]
async fn a_server_that_exits_during_initialize_fails_startup_with_its_stderr() {
    let fixture = fixture();
    let error = start_failure(&fixture, "echo 'boom: PROBE_KEY is not set' >&2; exit 3\n").await;

    let message = error.to_string();
    assert!(
        matches!(
            error,
            McpStartError::Handshake {
                step: "initialize",
                ..
            }
        ),
        "{message}"
    );
    assert!(message.contains("`broken`"), "names the server: {message}");
    assert!(
        message.contains("boom: PROBE_KEY is not set"),
        "carries the server's own explanation: {message}"
    );
}

#[tokio::test]
async fn an_initialize_error_response_fails_startup() {
    let fixture = fixture();
    let error = start_failure(
        &fixture,
        r#"read -r line
printf '{"jsonrpc":"2.0","id":1,"error":{"code":-32602,"message":"unsupported protocol"}}\n'
read -r line
"#,
    )
    .await;

    let message = error.to_string();
    assert!(message.contains("unsupported protocol"), "{message}");
}

#[tokio::test]
async fn an_empty_configuration_starts_nothing_and_needs_no_sandbox() {
    let config = McpConfig::parse(r#"{"servers": {}}"#).unwrap();
    let servers = start_servers(&config, SandboxRequirement::Isolated, Path::new("/"), &[])
        .await
        .unwrap();
    assert!(servers.is_empty());
}

#[tokio::test]
async fn a_requirement_the_platform_cannot_meet_fails_before_any_server_runs() {
    let fixture = fixture();
    let config = McpConfig::parse(&format!(
        r#"{{"servers": {{"probe": {{"command": "/bin/sh", "args": ["{}"]}}}}}}"#,
        fixture.script.display()
    ))
    .unwrap();

    // Stronger than Landlock; not implemented on any platform yet.
    let error = start_servers(
        &config,
        SandboxRequirement::Isolated,
        &fixture.workspace,
        &[],
    )
    .await
    .err()
    .expect("must refuse to run with less confinement than required");
    assert!(matches!(error, McpStartError::Sandbox { .. }), "{error:?}");
}
