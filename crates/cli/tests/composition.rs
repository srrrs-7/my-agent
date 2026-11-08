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
use agent_domain::model::tool::{ToolCall, ToolCallId, ToolName};
use agent_domain::ports::approval::{ApprovalDecision, ApprovalRequest};
use agent_domain::ports::command::CommandRunner as _;
use agent_domain::ports::events::{AgentEvent, EventSink};
use agent_domain::ports::tool::Tool;
use agent_infrastructure::config::{MapEnv, Settings};
use agent_test_support::{MockLlmServer, RecordingServer, Response};
use serde_json::{Value, json};
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
    /// Where operator files go: never inside the workspace.
    outside: TempDir,
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
        let outside = tempfile::tempdir().expect("a directory outside the workspace");
        Self {
            workspace,
            outside,
            env,
        }
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

    /// Writes `rules` to a file outside the workspace and points
    /// `AGENT_PERMISSIONS` at it.
    fn permissions(self, rules: &str) -> Self {
        let path = self.outside.path().join("permissions.json");
        std::fs::write(&path, rules).unwrap();
        let path = path.display().to_string();
        self.with("AGENT_PERMISSIONS", &path)
    }

    /// Writes `hooks` to a file outside the workspace and points
    /// `AGENT_HOOKS` at it. Unconfined: these tests are about where the
    /// hooks sit in the wiring, and `tests/hooks.rs` in the infrastructure
    /// crate proves the confinement on the platform that has it.
    fn hooks(self, hooks: Value) -> Self {
        let path = self.outside.path().join("hooks.json");
        std::fs::write(&path, hooks.to_string()).unwrap();
        let path = path.display().to_string();
        self.with("AGENT_HOOKS", &path)
            .with("AGENT_HOOKS_SANDBOX", "none")
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

// --- the approval stack ---------------------------------------------------------

/// What the whole approval stack says about one call of a registered tool.
async fn decide(app: &Application, name: &str, arguments: Value) -> ApprovalDecision {
    let request = ApprovalRequest {
        call: ToolCall::new(
            ToolCallId::new("1"),
            ToolName::new(name).unwrap(),
            arguments,
        ),
        safety: tool(app, name).definition().safety,
        summary: name.to_string(),
        must_ask: false,
    };
    app.approval.authorize(&request).await.unwrap()
}

fn refusal(decision: ApprovalDecision) -> String {
    match decision {
        ApprovalDecision::Deny { reason } => reason,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

const ALLOW_ALL_WRITES: &str = r#"{"rules": [{"tool": "write_file", "effect": "allow"}]}"#;

#[tokio::test]
async fn plan_mode_is_outside_the_rules_so_an_allow_cannot_get_past_it() {
    let setup = Setup::new().permissions(ALLOW_ALL_WRITES);
    let (app, _) = setup.assemble().await.unwrap();
    let write = json!({"path": "a.txt", "content": "x"});

    assert!(
        decide(&app, "write_file", write.clone())
            .await
            .is_approved(),
        "the rule approves without asking, although nobody could be asked"
    );

    app.plan_mode.set(&mut Session::new("s"), true);
    let reason = refusal(decide(&app, "write_file", write).await);
    assert!(reason.contains("plan mode is on"), "{reason}");
    assert!(
        decide(&app, "read_file", json!({"path": "a.txt"}))
            .await
            .is_approved(),
        "reading goes on to the ordinary policy, which approves it"
    );
}

#[tokio::test]
async fn an_ask_rule_reaches_the_human_gate_even_under_auto_approval() {
    let setup = Setup::new()
        .with("AGENT_APPROVAL", "auto")
        .permissions(r#"{"rules": [{"tool": "read_file", "path": "private/", "effect": "ask"}]}"#);
    let (app, _) = setup.assemble().await.unwrap();

    let reason = refusal(decide(&app, "read_file", json!({"path": "private/a.txt"})).await);
    assert!(
        reason.contains("permission rules require a human"),
        "auto approval answered for nobody: {reason}"
    );
    assert!(
        decide(&app, "read_file", json!({"path": "public/a.txt"}))
            .await
            .is_approved()
    );
}

#[cfg(unix)]
#[tokio::test]
async fn a_deny_rule_follows_a_link_to_the_file_it_protects() {
    let setup = Setup::new().with("AGENT_APPROVAL", "auto").permissions(
        r#"{"rules": [{"tool": "write_file", "path": "protected/", "effect": "deny"}]}"#,
    );
    std::fs::create_dir(setup.path().join("protected")).unwrap();
    std::fs::write(setup.path().join("protected/real.txt"), "keep").unwrap();
    std::os::unix::fs::symlink("protected/real.txt", setup.path().join("notes.txt")).unwrap();
    let (app, _) = setup.assemble().await.unwrap();

    let reason = refusal(
        decide(
            &app,
            "write_file",
            json!({"path": "notes.txt", "content": "overwritten"}),
        )
        .await,
    );
    assert!(
        reason.contains("deny write_file path `protected/`"),
        "{reason}"
    );
    assert!(
        decide(
            &app,
            "write_file",
            json!({"path": "other.txt", "content": "x"})
        )
        .await
        .is_approved()
    );
}

#[tokio::test]
async fn the_dependency_question_passes_through_every_layer_to_the_human_gate() {
    let llm = MockLlmServer::start(vec![
        Response::tool_call("call_1", "edit_file", ADD_TOKIO),
        Response::assistant_text("Done."),
    ])
    .await;
    let setup = Setup::new()
        .model(&llm)
        .with("AGENT_ALLOW_DEPENDENCY_CHANGES", "true")
        .permissions(r#"{"rules": [{"tool": "edit_file", "effect": "allow"}]}"#)
        .write("Cargo.toml", MANIFEST);
    let (app, _) = setup.assemble().await.unwrap();

    app.agent
        .run(&mut Session::new("composition-test"), "add tokio")
        .await
        .unwrap();

    let manifest = std::fs::read_to_string(setup.path().join("Cargo.toml")).unwrap();
    assert!(
        manifest.contains("tokio = \"1\""),
        "only the innermost gate knows the operator allowed it: {manifest}"
    );
}

#[tokio::test]
async fn a_rules_file_inside_the_workspace_stops_startup() {
    let setup = Setup::new().write("permissions.json", ALLOW_ALL_WRITES);
    let inside = setup.path().join("permissions.json").display().to_string();
    let error = setup
        .with("AGENT_PERMISSIONS", &inside)
        .assemble()
        .await
        .err()
        .expect("refused");
    let message = format!("{error:#}");
    assert!(message.contains("inside the workspace"), "{message}");
}

#[tokio::test]
async fn a_rule_that_allows_commands_stops_startup() {
    let error = Setup::new()
        .permissions(r#"{"rules": [{"tool": "run_command", "command": "ls", "effect": "allow"}]}"#)
        .assemble()
        .await
        .err()
        .expect("refused");
    let message = format!("{error:#}");
    assert!(
        message.contains("only be denied or asked about"),
        "{message}"
    );
}

#[tokio::test]
async fn web_fetch_deny_rules_also_refuse_a_redirect() {
    let server = MockLlmServer::start(vec![Response::redirect(
        "302 Found",
        "http://mirror.blocked.example/copy",
    )])
    .await;
    let setup = Setup::new()
        .with("AGENT_WEB_FETCH", "true")
        // The mock is on loopback.
        .with("AGENT_WEB_FETCH_ALLOW_PRIVATE", "true")
        .permissions(
            r#"{"rules": [{"tool": "web_fetch", "domain": "blocked.example", "effect": "deny"}]}"#,
        );
    let (app, _) = setup.assemble().await.unwrap();

    let reason = refusal(
        decide(
            &app,
            "web_fetch",
            json!({"url": "https://blocked.example/"}),
        )
        .await,
    );
    assert!(reason.contains("blocked.example"), "{reason}");

    // The gate never sees the redirect; the fetcher does.
    let error = tool(&app, "web_fetch")
        .execute(json!({"url": server.base_url()}))
        .await
        .expect_err("the redirect is refused");
    assert!(error.to_string().contains("permission rules"), "{error}");
}

// --- hooks ----------------------------------------------------------------------

/// A pre hook on every tool that leaves `marker` in the workspace and then
/// exits with `code`, saying `said` on stderr.
fn pre_hook(marker: &str, code: i32, said: &str) -> Value {
    json!({"pre_tool_use": [{
        "match": "*",
        "command": format!("touch {marker}; echo '{said}' >&2; exit {code}"),
    }]})
}

#[tokio::test]
async fn plan_mode_refuses_before_any_hook_is_started() {
    let setup = Setup::new().hooks(pre_hook("hook-ran", 0, ""));
    let (app, _) = setup.assemble().await.unwrap();

    app.plan_mode.set(&mut Session::new("s"), true);
    let reason = refusal(decide(&app, "write_file", json!({"path": "a", "content": "x"})).await);

    assert!(reason.contains("plan mode is on"), "{reason}");
    assert!(
        !setup.path().join("hook-ran").exists(),
        "plan mode is outermost: no hook runs for a call that cannot run anyway"
    );
}

#[tokio::test]
async fn a_hook_refuses_even_what_an_allow_rule_lets_through() {
    let setup = Setup::new().permissions(ALLOW_ALL_WRITES).hooks(pre_hook(
        "hook-ran",
        1,
        "the hook says no",
    ));
    let (app, _) = setup.assemble().await.unwrap();

    let reason = refusal(decide(&app, "write_file", json!({"path": "a", "content": "x"})).await);

    assert_eq!(
        reason, "the hook says no",
        "the hooks sit outside the rules"
    );
}

#[tokio::test]
async fn a_passing_hook_still_leaves_the_decision_to_the_rules() {
    let setup = Setup::new()
        .permissions(r#"{"rules": [{"tool": "write_file", "effect": "deny"}]}"#)
        .hooks(pre_hook("hook-ran", 0, ""));
    let (app, _) = setup.assemble().await.unwrap();

    let reason = refusal(decide(&app, "write_file", json!({"path": "a", "content": "x"})).await);

    assert!(setup.path().join("hook-ran").exists(), "the hook ran first");
    assert!(
        reason.contains("permission rule `deny write_file`"),
        "exit 0 approves nothing: {reason}"
    );
}

#[tokio::test]
async fn with_hooks_the_dependency_question_still_reaches_the_human_gate() {
    let llm = MockLlmServer::start(vec![
        Response::tool_call("call_1", "edit_file", ADD_TOKIO),
        Response::assistant_text("Done."),
    ])
    .await;
    let setup = Setup::new()
        .model(&llm)
        .with("AGENT_APPROVAL", "auto")
        .with("AGENT_ALLOW_DEPENDENCY_CHANGES", "true")
        .hooks(pre_hook("hook-ran", 0, ""))
        .write("Cargo.toml", MANIFEST);
    let (app, _) = setup.assemble().await.unwrap();

    app.agent
        .run(&mut Session::new("composition-test"), "add tokio")
        .await
        .unwrap();

    let manifest = std::fs::read_to_string(setup.path().join("Cargo.toml")).unwrap();
    assert!(
        manifest.contains("tokio = \"1\""),
        "the hook layer must forward the question, not answer it: {manifest}"
    );
}

#[tokio::test]
async fn a_post_hook_runs_after_the_call_through_the_registered_tool() {
    let setup = Setup::new().hooks(json!({"post_tool_use": [{
        "match": "write_file",
        "command": "cat > post.json",
    }]}));
    let (app, _) = setup.assemble().await.unwrap();

    tool(&app, "write_file")
        .execute(json!({"path": "a.txt", "content": "x"}))
        .await
        .unwrap();

    let seen: Value =
        serde_json::from_str(&std::fs::read_to_string(setup.path().join("post.json")).unwrap())
            .unwrap();
    assert_eq!(seen["event"], "post_tool_use");
    assert_eq!(seen["tool"], "write_file");
    assert_eq!(seen["is_error"], false);
}

#[tokio::test]
async fn a_hooks_file_inside_the_workspace_stops_startup() {
    let setup = Setup::new().write("hooks.json", "{}");
    let inside = setup.path().join("hooks.json").display().to_string();
    let error = setup
        .with("AGENT_HOOKS", &inside)
        .assemble()
        .await
        .err()
        .expect("refused");
    let message = format!("{error:#}");
    assert!(message.contains("AGENT_HOOKS"), "{message}");
    assert!(message.contains("inside the workspace"), "{message}");
}

#[tokio::test]
async fn a_hook_on_a_tool_that_is_not_offered_stops_startup() {
    // `run_command` is not offered without AGENT_SHELL: a hook meant to
    // police it would never run, and nobody would notice.
    let error = Setup::new()
        .hooks(json!({"pre_tool_use": [{"match": "run_command", "command": "exit 1"}]}))
        .assemble()
        .await
        .err()
        .expect("refused");
    let message = format!("{error:#}");
    assert!(message.contains("run_command"), "{message}");
    assert!(message.contains("not offered"), "{message}");
}

#[tokio::test]
async fn an_unknown_field_in_the_hooks_file_stops_startup() {
    let error = Setup::new()
        .hooks(json!({"pre_tool_use": [{"match": "*", "command": "exit 0", "on_fail": "allow"}]}))
        .assemble()
        .await
        .err()
        .expect("refused");
    let message = format!("{error:#}");
    assert!(message.contains("unknown field"), "{message}");
}

/// As for the shell: either outcome is legitimate, but they must not mix.
#[tokio::test]
async fn hooks_under_the_default_sandbox_either_confine_or_stop_startup() {
    let setup = Setup::new().hooks(json!({"stop": [{"command": "exit 0"}]}));
    // The default, not the `none` the other tests use.
    let result = setup
        .with("AGENT_HOOKS_SANDBOX", "confined")
        .assemble()
        .await;

    match result {
        Ok((app, _)) => {
            let hooks = app.hooks.as_ref().expect("the hooks were started");
            assert!(
                hooks.sandbox().confines(),
                "started with {:?}, which does not confine",
                hooks.sandbox()
            );
        }
        Err(error) => {
            let message = format!("{error:#}");
            assert!(message.contains("AGENT_HOOKS_SANDBOX"), "{message}");
        }
    }
}
