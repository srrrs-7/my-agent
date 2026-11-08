//! The composition root, built the way `agent` builds it.
//!
//! `end_to_end.rs` wires the loop by hand, so it cannot see a tool that was
//! never registered, a decorator left off, or an approval setting wired to
//! the wrong place. These tests call [`composition::assemble`] - the function
//! every subcommand goes through - with settings injected through `MapEnv`
//! (never the process environment) and a temporary workspace, and check what
//! came out. The model, where one is needed, is a loopback mock.

use std::path::Path;
use std::sync::{Arc, Mutex};

use agent_application::agent::Session;
use agent_cli::composition::{self, Application, Wiring};
use agent_domain::model::tool::ToolName;
use agent_domain::ports::command::CommandRunner as _;
use agent_domain::ports::events::{AgentEvent, EventSink};
use agent_domain::ports::tool::Tool;
use agent_infrastructure::config::{MapEnv, Settings};
use agent_test_support::{MockLlmServer, RecordingServer, Response};
use serde_json::json;
use tempfile::TempDir;

/// Distinctive, and long enough for the redactor to take it as a secret.
const API_KEY: &str = "sk-test-0123456789abcdef";

const MANIFEST: &str = "[package]\nname = \"demo\"\n\n[dependencies]\nserde = \"1\"\n";

const ADD_TOKIO: &str = r#"{"path":"Cargo.toml","old_string":"serde = \"1\"\n","new_string":"serde = \"1\"\ntokio = \"1\"\n"}"#;

/// Records which events arrived, by kind, for the assertions that need to
/// know whether the dependency watch fired.
#[derive(Default)]
struct RecordingSink {
    dependency_reports: Mutex<usize>,
}

impl RecordingSink {
    fn dependency_reports(&self) -> usize {
        *self.dependency_reports.lock().unwrap()
    }
}

impl EventSink for RecordingSink {
    fn emit(&self, event: AgentEvent) {
        if matches!(event, AgentEvent::DependenciesChanged { .. }) {
            *self.dependency_reports.lock().unwrap() += 1;
        }
    }
}

/// A temporary workspace and the settings that point at it, built up before
/// the agent is assembled.
struct Setup {
    workspace: TempDir,
    env: MapEnv,
}

impl Setup {
    /// The minimum the agent starts with. The base URL leads nowhere unless a
    /// test replaces it: assembling never contacts the model.
    fn new() -> Self {
        let workspace = tempfile::tempdir().expect("a temporary workspace");
        let mut env = MapEnv::new(&[
            ("AGENT_PROVIDER", "openai"),
            ("AGENT_BASE_URL", "http://127.0.0.1:9/v1"),
            ("AGENT_MODEL", "mock"),
            ("AGENT_STREAM", "false"),
            ("AGENT_MAX_RETRIES", "0"),
        ]);
        env.set("AGENT_WORKSPACE", &workspace.path().display().to_string());
        Self { workspace, env }
    }

    fn with(mut self, key: &str, value: &str) -> Self {
        self.env.set(key, value);
        self
    }

    fn model(self, llm: &MockLlmServer) -> Self {
        let url = llm.base_url().to_string();
        self.with("AGENT_BASE_URL", &url)
    }

    fn path(&self) -> &Path {
        self.workspace.path()
    }

    fn write(self, name: &str, contents: &str) -> Self {
        std::fs::write(self.workspace.path().join(name), contents).unwrap();
        self
    }

    /// Unattended, as in CI: nobody can answer a prompt.
    async fn assemble(&self) -> anyhow::Result<(Application, Arc<RecordingSink>)> {
        let settings = Settings::from_source(&self.env).expect("valid settings");
        let events = Arc::new(RecordingSink::default());
        let app = composition::assemble(
            settings,
            Wiring {
                interactive: false,
                events: events.clone(),
                model: None,
                environment: Vec::new(),
                retention: None,
            },
        )
        .await?;
        Ok((app, events))
    }
}

fn tool(app: &Application, name: &str) -> Arc<dyn Tool> {
    app.tools
        .get(&ToolName::new(name).unwrap())
        .unwrap_or_else(|| panic!("`{name}` is not registered: {:?}", app.tools.names()))
}

fn sorted_names(app: &Application) -> Vec<String> {
    let mut names = app.tools.names();
    names.sort();
    names
}

const DEFAULT_TOOLS: [&str; 7] = [
    "edit_file",
    "glob",
    "list_directory",
    "read_file",
    "search_files",
    "todo_write",
    "write_file",
];

// --- which tools are offered ---------------------------------------------------

#[tokio::test]
async fn default_settings_offer_exactly_the_file_and_plan_tools() {
    let (app, _) = Setup::new().assemble().await.unwrap();

    assert_eq!(sorted_names(&app), DEFAULT_TOOLS);
    assert!(
        app.commands.is_none(),
        "no command runner without AGENT_SHELL"
    );
}

#[tokio::test]
async fn web_fetch_is_offered_only_when_the_operator_enables_it() {
    let (app, _) = Setup::new()
        .with("AGENT_WEB_FETCH", "true")
        .assemble()
        .await
        .unwrap();

    let mut expected: Vec<&str> = DEFAULT_TOOLS.to_vec();
    expected.push("web_fetch");
    expected.sort_unstable();
    assert_eq!(sorted_names(&app), expected);
}

#[tokio::test]
async fn web_search_is_not_offered_on_a_key_alone() {
    let (app, _) = Setup::new()
        .with("AGENT_WEB_SEARCH_API_KEY", "BSA-0123456789abcdef")
        .assemble()
        .await
        .unwrap();

    assert_eq!(sorted_names(&app), DEFAULT_TOOLS);
}

#[tokio::test]
async fn web_search_is_offered_when_the_operator_enables_it_and_asks_first() {
    let (app, _) = Setup::new()
        .with("AGENT_WEB_SEARCH", "true")
        .with("AGENT_WEB_SEARCH_API_KEY", "BSA-0123456789abcdef")
        .assemble()
        .await
        .unwrap();

    let mut expected: Vec<&str> = DEFAULT_TOOLS.to_vec();
    expected.push("web_search");
    expected.sort_unstable();
    assert_eq!(sorted_names(&app), expected);
    assert!(
        !tool(&app, "web_search").definition().safety.is_read_only(),
        "a query leaves the machine, so the default policy must ask"
    );
}

#[test]
fn enabling_web_search_without_a_key_stops_startup() {
    let setup = Setup::new().with("AGENT_WEB_SEARCH", "true");
    let error =
        Settings::from_source(&setup.env).expect_err("searching without a key must not start");
    assert!(
        error.to_string().contains("AGENT_WEB_SEARCH_API_KEY"),
        "{error}"
    );
}

#[tokio::test]
async fn run_command_is_offered_only_when_the_operator_enables_the_shell() {
    // `none` is the one requirement every platform satisfies, so this pins
    // the registration rather than the sandbox.
    let (app, _) = Setup::new()
        .with("AGENT_SHELL", "true")
        .with("AGENT_SHELL_SANDBOX", "none")
        .assemble()
        .await
        .unwrap();

    let mut expected: Vec<&str> = DEFAULT_TOOLS.to_vec();
    expected.push("run_command");
    expected.sort_unstable();
    assert_eq!(sorted_names(&app), expected);
    assert!(
        app.commands.is_some(),
        "doctor reads the sandbox from the runner, so it must be kept"
    );
}

/// `isolated` is implemented on no platform yet, so it is the requirement
/// that cannot be met anywhere this suite runs - which makes it the way to
/// show that startup refuses rather than registering a weaker tool.
#[tokio::test]
async fn a_sandbox_that_cannot_be_had_stops_startup() {
    let error = Setup::new()
        .with("AGENT_SHELL", "true")
        .with("AGENT_SHELL_SANDBOX", "isolated")
        .assemble()
        .await
        .err()
        .expect("startup must fail rather than run commands unconfined");

    let message = format!("{error:#}");
    assert!(message.contains("AGENT_SHELL_SANDBOX"), "{message}");
    assert!(message.contains("AGENT_SHELL=false"), "{message}");
}

/// The default requirement depends on the kernel, so either outcome is
/// legitimate here - but they must not mix: if the agent starts, the runner
/// it registered really confines.
#[tokio::test]
async fn the_default_sandbox_either_confines_or_stops_startup() {
    let result = Setup::new().with("AGENT_SHELL", "true").assemble().await;

    match result {
        Ok((app, _)) => {
            let runner = app.commands.as_ref().expect("a runner was registered");
            assert!(
                runner.sandbox().confines(),
                "registered with {:?}, which does not confine",
                runner.sandbox()
            );
            assert!(app.tools.names().contains(&"run_command".to_string()));
        }
        Err(error) => {
            let message = format!("{error:#}");
            assert!(message.contains("AGENT_SHELL_SANDBOX"), "{message}");
        }
    }
}

// --- what every tool is wrapped in ---------------------------------------------

#[tokio::test]
async fn a_known_secret_in_tool_output_reaches_the_model_redacted() {
    let setup = Setup::new()
        .with("AGENT_API_KEY", API_KEY)
        .write("leak.txt", &format!("token = {API_KEY}\n"));
    let (app, _) = setup.assemble().await.unwrap();

    let outcome = tool(&app, "read_file")
        .execute(json!({"path": "leak.txt"}))
        .await
        .unwrap();

    assert!(!outcome.content.contains(API_KEY), "{}", outcome.content);
    assert!(
        outcome.content.contains("[REDACTED]"),
        "{}",
        outcome.content
    );
}

/// A server that never answers holds the fetch open, so the only thing that
/// can end the call is the tool timeout the composition root puts around it.
#[tokio::test]
async fn the_tool_timeout_cuts_off_a_call_that_never_returns() {
    let silent = RecordingServer::unresponsive().await;
    let (app, _) = Setup::new()
        .with("AGENT_WEB_FETCH", "true")
        .with("AGENT_WEB_FETCH_ALLOW_PRIVATE", "true")
        .with("AGENT_TOOL_TIMEOUT_SECS", "0")
        .assemble()
        .await
        .unwrap();

    let error = tool(&app, "web_fetch")
        .execute(json!({"url": format!("{}/page", silent.url())}))
        .await
        .expect_err("the call must be cut off");

    assert!(error.to_string().contains("timed out after 0s"), "{error}");
}

/// A command can rewrite a manifest without saying so, so the destructive
/// tool is the one watched. `none` keeps this about the wiring, not the
/// sandbox.
#[tokio::test]
async fn a_destructive_tool_reports_a_change_to_a_dependency_file() {
    let setup = Setup::new()
        .with("AGENT_SHELL", "true")
        .with("AGENT_SHELL_SANDBOX", "none")
        .write("Cargo.toml", MANIFEST);
    let (app, events) = setup.assemble().await.unwrap();

    let outcome = tool(&app, "run_command")
        .execute(json!({"command": "echo 'tokio = \"1\"' >> Cargo.toml"}))
        .await
        .unwrap();

    let manifest = std::fs::read_to_string(setup.path().join("Cargo.toml")).unwrap();
    assert!(manifest.contains("tokio"), "the command ran: {manifest}");
    assert_eq!(events.dependency_reports(), 1, "{}", outcome.content);
}

/// `edit_file` says what it will write, so a manifest change through it is
/// approved beforehand (see the turn tests below) rather than reported after.
#[tokio::test]
async fn a_file_tool_is_not_watched() {
    let setup = Setup::new().write("Cargo.toml", MANIFEST);
    let (app, events) = setup.assemble().await.unwrap();

    tool(&app, "edit_file")
        .execute(serde_json::from_str(ADD_TOKIO).unwrap())
        .await
        .unwrap();

    let manifest = std::fs::read_to_string(setup.path().join("Cargo.toml")).unwrap();
    assert!(manifest.contains("tokio"), "the edit applied: {manifest}");
    assert_eq!(events.dependency_reports(), 0);
}

// --- a turn through the real wiring ------------------------------------------

async fn manifest_after_an_unattended_edit(allow_dependency_changes: &str) -> String {
    let llm = MockLlmServer::start(vec![
        Response::tool_call("call_1", "edit_file", ADD_TOKIO),
        Response::assistant_text("Done."),
    ])
    .await;
    let setup = Setup::new()
        .model(&llm)
        .with("AGENT_APPROVAL", "auto")
        .with("AGENT_ALLOW_DEPENDENCY_CHANGES", allow_dependency_changes)
        .write("Cargo.toml", MANIFEST);
    let (app, _) = setup.assemble().await.unwrap();

    app.agent
        .run(&mut Session::new("composition-test"), "add tokio")
        .await
        .expect("a refused call is a tool result, not a failed turn");

    std::fs::read_to_string(setup.path().join("Cargo.toml")).unwrap()
}

#[tokio::test]
async fn auto_approval_alone_does_not_let_a_dependency_change_through() {
    assert_eq!(manifest_after_an_unattended_edit("false").await, MANIFEST);
}

#[tokio::test]
async fn the_operator_can_allow_dependency_changes_ahead_of_time() {
    let manifest = manifest_after_an_unattended_edit("true").await;
    assert!(manifest.contains("tokio = \"1\""), "{manifest}");
}

/// Every file under `dir`, recursively. Empty when `dir` does not exist.
fn files_under(dir: &Path) -> Vec<std::path::PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .flat_map(|entry| {
            let path = entry.path();
            if path.is_dir() {
                files_under(&path)
            } else {
                vec![path]
            }
        })
        .collect()
}

async fn session_files_after_one_turn(log: &str) -> Vec<std::path::PathBuf> {
    let llm = MockLlmServer::start(vec![Response::assistant_text("Hello.")]).await;
    let setup = Setup::new().model(&llm).with("AGENT_SESSION_LOG", log);
    let (app, _) = setup.assemble().await.unwrap();

    app.agent
        .run(&mut Session::new("composition-test"), "hi")
        .await
        .unwrap();

    files_under(&app.settings.session_dir())
}

#[tokio::test]
async fn the_session_log_records_a_turn_when_on() {
    let files = session_files_after_one_turn("true").await;
    assert_eq!(files.len(), 1, "{files:?}");
}

#[tokio::test]
async fn the_session_log_writes_nothing_when_off() {
    let files = session_files_after_one_turn("false").await;
    assert!(files.is_empty(), "{files:?}");
}
