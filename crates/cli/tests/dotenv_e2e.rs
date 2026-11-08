//! `.env` through the real binary.
//!
//! Which keys `.env` may set is decided by a pure function with its own
//! tests; what only the binary shows is that `main` goes through it. So this
//! starts `agent tools` - which assembles the agent and asks the model
//! nothing - from a directory holding a `.env`, with a cleared environment
//! (no `std::env::set_var`), and reads which tools came out.

use std::path::Path;
use std::process::{Output, Stdio};
use std::time::Duration;

use tokio::process::Command;

/// Only here so a hang fails the test instead of the suite.
const DEADLINE: Duration = Duration::from_secs(60);

/// The connection settings are allowed from `.env`; `AGENT_WEB_FETCH` is not.
const DOTENV: &str = "AGENT_PROVIDER=openai\n\
                      AGENT_BASE_URL=http://127.0.0.1:9/v1\n\
                      AGENT_MODEL=mock\n\
                      AGENT_WEB_FETCH=true\n";

async fn tools(dir: &Path, env: &[(&str, &str)]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_agent"));
    command
        .env_clear()
        .env("AGENT_WORKSPACE", dir)
        .envs(env.iter().copied())
        .current_dir(dir)
        .arg("--no-color")
        .arg("tools")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    tokio::time::timeout(DEADLINE, command.output())
        .await
        .unwrap_or_else(|_| panic!("agent tools was still running after {DEADLINE:?}"))
        .expect("run the agent binary")
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

#[tokio::test]
async fn a_dotenv_cannot_turn_on_a_tool_but_still_configures_the_connection() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join(".env"), DOTENV).unwrap();

    let output = tools(dir.path(), &[]).await;
    let stdout = text(&output.stdout);
    let stderr = text(&output.stderr);

    assert!(
        output.status.success(),
        "the provider settings came from .env\n--- stderr\n{stderr}"
    );
    assert!(!stdout.contains("web_fetch"), "{stdout}");
    assert!(
        stderr.contains("AGENT_WEB_FETCH"),
        "the operator is told what was ignored: {stderr}"
    );
}

#[tokio::test]
async fn the_real_environment_still_turns_it_on() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join(".env"), DOTENV).unwrap();

    let output = tools(dir.path(), &[("AGENT_WEB_FETCH", "true")]).await;
    let stdout = text(&output.stdout);
    let stderr = text(&output.stderr);

    assert!(output.status.success(), "--- stderr\n{stderr}");
    assert!(stdout.contains("web_fetch"), "{stdout}");
    assert!(
        !stderr.contains("ignored"),
        "nothing was ignored: the real value stands: {stderr}"
    );
}
