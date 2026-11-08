//! MCP servers through the real binary.
//!
//! The pieces meet only in the composition root, which has no harness of its
//! own: the configuration is read and checked against the workspace, the
//! server values join the redactor, the servers start under the sandbox, and
//! their tools are wrapped in the timeout, the dependency watch and the
//! redactor - in that order - before the approval gate sees them. So these
//! start `agent` itself, against a scripted loopback model and a `/bin/sh`
//! MCP server, and check what came out the other end.
//!
//! Linux only: the servers need the Landlock sandbox.

#![cfg(target_os = "linux")]

use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};
use std::time::{Duration, Instant};

use agent_test_support::{MockLlmServer, Response};
use tokio::process::Command;

/// Only here so a hang fails the test instead of the suite.
const DEADLINE: Duration = Duration::from_secs(90);

/// Handed to the server through the configuration. Long enough to count as a
/// secret for the redactor.
const SECRET: &str = "mcp-secret-0123456789";

/// A tiny MCP server. `$1` is the workspace, for the tool that writes there.
const SERVER: &str = r#"
ws="$1"
reply() { printf '{"jsonrpc":"2.0","id":%s,"result":%s}\n' "$1" "$2"; }
text() { reply "$1" "{\"content\":[{\"type\":\"text\",\"text\":\"$2\"}]}"; }

while IFS= read -r line; do
  id=$(printf '%s\n' "$line" | grep -o '"id":[0-9]*' | head -n 1 | cut -d: -f2)
  [ -z "$id" ] && continue
  case "$line" in
    *'"method":"initialize"'*)
      reply "$id" '{"protocolVersion":"2025-06-18","capabilities":{"tools":{}},"serverInfo":{"name":"probe","version":"0"}}' ;;
    *'"method":"tools/list"'*)
      reply "$id" '{"tools":[{"name":"echo","description":"Says what it was given.","inputSchema":{"type":"object"},"annotations":{"readOnlyHint":true}},{"name":"hang"},{"name":"bump"},{"name":"spawn"}]}' ;;
    *'"method":"tools/call"'*)
      case "$line" in
        *'"name":"echo"'*) text "$id" "token=$PROBE_TOKEN" ;;
        *'"name":"hang"'*) : ;;
        *'"name":"bump"'*)
          printf 'tokio = "1"\n' >> "$ws/Cargo.toml"
          text "$id" bumped ;;
        *'"name":"spawn"'*)
          sleep 100 >/dev/null 2>&1 &
          text "$id" "pid=$!" ;;
      esac ;;
  esac
done
"#;

struct Fixture {
    workspace: tempfile::TempDir,
    /// Outside the workspace: the configuration and the server script.
    outside: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        let fixture = Self {
            workspace: tempfile::tempdir().unwrap(),
            outside: tempfile::tempdir().unwrap(),
        };
        std::fs::write(fixture.outside.path().join("server.sh"), SERVER).unwrap();
        std::fs::write(
            fixture.workspace.path().join("Cargo.toml"),
            "[dependencies]\n",
        )
        .unwrap();
        fixture
    }

    fn workspace(&self) -> PathBuf {
        std::fs::canonicalize(self.workspace.path()).unwrap()
    }

    /// Writes a configuration for the probe server to `dir` and returns its
    /// path.
    fn config_in(&self, dir: &Path, workspace_access: &str) -> PathBuf {
        let script = self.outside.path().join("server.sh");
        let config = serde_json::json!({
            "servers": {
                "probe": {
                    "command": "/bin/sh",
                    "args": [script, self.workspace()],
                    "env": {"PROBE_TOKEN": SECRET},
                    "workspace_access": workspace_access,
                }
            }
        });
        let path = dir.join("mcp.json");
        std::fs::write(&path, config.to_string()).unwrap();
        path
    }

    fn config(&self, workspace_access: &str) -> PathBuf {
        self.config_in(self.outside.path(), workspace_access)
    }
}

async fn agent(
    workspace: &Path,
    llm: Option<&MockLlmServer>,
    env: &[(&str, &str)],
    args: &[&str],
) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_agent"));
    command
        .env_clear()
        // `sh`, `grep` and `sleep` are looked up on PATH inside the server.
        .env("PATH", "/usr/local/bin:/usr/bin:/bin")
        .env("AGENT_WORKSPACE", workspace)
        .env("AGENT_PROVIDER", "openai")
        .env(
            "AGENT_BASE_URL",
            llm.map_or("http://127.0.0.1:9", MockLlmServer::base_url),
        )
        .env("AGENT_MODEL", "mock")
        .env("AGENT_STREAM", "false")
        .env("AGENT_MAX_RETRIES", "0")
        .env("AGENT_SESSION_LOG", "false")
        .envs(env.iter().copied())
        .current_dir(workspace)
        .arg("--no-color")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    tokio::time::timeout(DEADLINE, command.output())
        .await
        .unwrap_or_else(|_| panic!("the agent was still running after {DEADLINE:?}"))
        .expect("run the agent binary")
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn assert_success(output: &Output) {
    assert_eq!(
        output.status.code(),
        Some(0),
        "exit code\n--- stdout\n{}\n--- stderr\n{}",
        text(&output.stdout),
        text(&output.stderr)
    );
}

/// The content of the tool result the model received in its second request.
async fn tool_result(llm: MockLlmServer) -> String {
    let requests = llm.json_requests().await;
    requests[1]["messages"]
        .as_array()
        .expect("messages")
        .iter()
        .find(|message| message["role"] == "tool")
        .expect("the call was answered")["content"]
        .as_str()
        .expect("text content")
        .to_string()
}

/// The model calls `tool` once, then answers.
async fn one_call(tool: &str) -> MockLlmServer {
    MockLlmServer::start(vec![
        Response::tool_call("call_1", tool, "{}"),
        Response::assistant_text("done"),
    ])
    .await
}

#[tokio::test]
async fn without_a_configuration_no_server_tool_is_offered() {
    let fixture = Fixture::new();
    let output = agent(&fixture.workspace(), None, &[], &["tools"]).await;
    assert_success(&output);
    assert!(
        !text(&output.stdout).contains("mcp__"),
        "{}",
        text(&output.stdout)
    );
}

#[tokio::test]
async fn configured_server_tools_are_listed_as_destructive_with_their_origin() {
    let fixture = Fixture::new();
    let config = fixture.config("none");
    let output = agent(
        &fixture.workspace(),
        None,
        &[("AGENT_MCP_CONFIG", config.to_str().unwrap())],
        &["tools"],
    )
    .await;
    assert_success(&output);

    let listing = text(&output.stdout);
    assert!(
        listing.contains("mcp__probe__echo [destructive]"),
        "a server's readOnlyHint is not believed: {listing}"
    );
    assert!(
        listing.contains("[MCP server probe] Says what it was given."),
        "{listing}"
    );
    for name in ["mcp__probe__hang", "mcp__probe__bump", "mcp__probe__spawn"] {
        assert!(listing.contains(name), "{name} missing: {listing}");
    }
}

#[tokio::test]
async fn a_value_handed_to_a_server_is_redacted_from_what_it_returns() {
    let fixture = Fixture::new();
    let config = fixture.config("none");
    let llm = one_call("mcp__probe__echo").await;

    let output = agent(
        &fixture.workspace(),
        Some(&llm),
        &[
            ("AGENT_MCP_CONFIG", config.to_str().unwrap()),
            ("AGENT_APPROVAL", "auto"),
        ],
        &["run", "echo"],
    )
    .await;
    assert_success(&output);

    let result = tool_result(llm).await;
    assert!(
        result.contains("token=[REDACTED]"),
        "the call ran and its output was redacted: {result}"
    );
    assert!(!result.contains(SECRET), "{result}");
}

#[tokio::test]
async fn a_call_that_never_returns_becomes_a_timeout_result() {
    let fixture = Fixture::new();
    let config = fixture.config("none");
    let llm = one_call("mcp__probe__hang").await;

    let output = agent(
        &fixture.workspace(),
        Some(&llm),
        &[
            ("AGENT_MCP_CONFIG", config.to_str().unwrap()),
            ("AGENT_APPROVAL", "auto"),
            ("AGENT_TOOL_TIMEOUT_SECS", "1"),
        ],
        &["run", "hang"],
    )
    .await;
    assert_success(&output);

    let result = tool_result(llm).await;
    assert!(result.contains("timed out"), "{result}");
}

#[tokio::test]
async fn a_server_allowed_into_the_workspace_is_watched_for_dependency_changes() {
    let fixture = Fixture::new();
    let config = fixture.config("read-write");
    let llm = one_call("mcp__probe__bump").await;

    let output = agent(
        &fixture.workspace(),
        Some(&llm),
        &[
            ("AGENT_MCP_CONFIG", config.to_str().unwrap()),
            ("AGENT_APPROVAL", "auto"),
        ],
        &["run", "bump"],
    )
    .await;
    assert_success(&output);

    let manifest = std::fs::read_to_string(fixture.workspace().join("Cargo.toml")).unwrap();
    assert!(manifest.contains("tokio"), "the server wrote: {manifest}");
    let result = tool_result(llm).await;
    assert!(
        result.starts_with("[dependency files changed by this call: Cargo.toml"),
        "{result}"
    );
}

#[tokio::test]
async fn a_server_without_workspace_access_cannot_change_the_manifest() {
    let fixture = Fixture::new();
    let config = fixture.config("none");
    let llm = one_call("mcp__probe__bump").await;

    let output = agent(
        &fixture.workspace(),
        Some(&llm),
        &[
            ("AGENT_MCP_CONFIG", config.to_str().unwrap()),
            ("AGENT_APPROVAL", "auto"),
        ],
        &["run", "bump"],
    )
    .await;
    assert_success(&output);

    let manifest = std::fs::read_to_string(fixture.workspace().join("Cargo.toml")).unwrap();
    assert_eq!(manifest, "[dependencies]\n");
}

#[tokio::test]
async fn a_server_tool_needs_approval_under_the_default_policy() {
    let fixture = Fixture::new();
    let config = fixture.config("none");
    let llm = one_call("mcp__probe__echo").await;

    // Default policy, stdin not a terminal: nobody can approve.
    let output = agent(
        &fixture.workspace(),
        Some(&llm),
        &[("AGENT_MCP_CONFIG", config.to_str().unwrap())],
        &["run", "echo"],
    )
    .await;
    assert_success(&output);

    let stderr = text(&output.stderr);
    assert!(stderr.contains("mcp__probe__echo denied"), "{stderr}");
    let result = tool_result(llm).await;
    assert!(
        !result.contains("token="),
        "the server was not called: {result}"
    );
}

/// Running, or at least not yet a zombie. Read from `/proc` rather than with
/// `kill(pid, 0)`, which would need `libc` in this crate's tests.
fn alive(pid: i32) -> bool {
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        // The state follows the parenthesised command name.
        Ok(stat) => stat
            .rsplit_once(')')
            .is_some_and(|(_, rest)| !rest.trim_start().starts_with('Z')),
        Err(_) => false,
    }
}

#[tokio::test]
async fn nothing_the_server_started_outlives_the_agent() {
    let fixture = Fixture::new();
    let config = fixture.config("none");
    let llm = one_call("mcp__probe__spawn").await;

    let output = agent(
        &fixture.workspace(),
        Some(&llm),
        &[
            ("AGENT_MCP_CONFIG", config.to_str().unwrap()),
            ("AGENT_APPROVAL", "auto"),
        ],
        &["run", "spawn"],
    )
    .await;
    assert_success(&output);

    let result = tool_result(llm).await;
    let grandchild: i32 = result
        .strip_prefix("pid=")
        .and_then(|pid| pid.trim().parse().ok())
        .unwrap_or_else(|| panic!("expected `pid=N`, got {result}"));

    // SIGKILL is asynchronous and the orphan is reaped by init: a bounded
    // poll on process state.
    let deadline = Instant::now() + Duration::from_secs(20);
    while alive(grandchild) {
        assert!(
            Instant::now() < deadline,
            "grandchild {grandchild} outlived the agent"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn a_configuration_inside_the_workspace_fails_startup() {
    let fixture = Fixture::new();
    let config = fixture.config_in(&fixture.workspace(), "none");
    let output = agent(
        &fixture.workspace(),
        None,
        &[("AGENT_MCP_CONFIG", config.to_str().unwrap())],
        &["tools"],
    )
    .await;

    assert_ne!(output.status.code(), Some(0));
    let stderr = text(&output.stderr);
    assert!(stderr.contains("inside the workspace"), "{stderr}");
}

#[tokio::test]
async fn a_server_that_fails_to_initialize_fails_startup() {
    let fixture = Fixture::new();
    std::fs::write(
        fixture.outside.path().join("server.sh"),
        "echo 'missing PROBE_KEY' >&2; exit 2\n",
    )
    .unwrap();
    let config = fixture.config("none");
    let output = agent(
        &fixture.workspace(),
        None,
        &[("AGENT_MCP_CONFIG", config.to_str().unwrap())],
        &["tools"],
    )
    .await;

    assert_ne!(output.status.code(), Some(0));
    let stderr = text(&output.stderr);
    assert!(
        stderr.contains("MCP server `probe`") && stderr.contains("missing PROBE_KEY"),
        "{stderr}"
    );
}
