//! Full-stack test: HTTP -> provider -> routing -> retry -> agent loop -> tools
//! -> real filesystem.
//!
//! Every layer of the clean architecture participates; only the model itself is
//! a stand-in (a mock server replaying a scripted two-turn conversation). This
//! is the test that would catch a mistake no single-layer test can see - a
//! payload that decodes fine but carries the wrong tool id, a sandbox check
//! that rejects a path the loop legitimately produced, and so on.

use std::sync::Arc;
use std::time::Duration;

use agent_application::agent::{
    AgentDependencies, AgentLoop, AgentLoopConfig, DefaultPromptBuilder, FixedPromptBuilder,
    Session,
};
use agent_application::tools::ToolRegistry;
use agent_application::tools::file::{ReadFileTool, WriteFileTool};
use agent_domain::error::ApprovalError;
use agent_domain::model::llm::{ModelId, ProviderId};
use agent_domain::model::message::Message;
use agent_domain::model::workspace::WorkspaceRoot;
use agent_domain::ports::approval::{ApprovalDecision, ApprovalGate, ApprovalRequest};
use agent_domain::ports::events::{FinishReason, NullEventSink};
use agent_domain::ports::prompt::PromptBuilder;
use agent_infrastructure::config::{LlmSettings, ProviderKind, ProviderSettings, RouterKind};
use agent_infrastructure::fs::{LocalFileSystem, WorkspaceContextProvider};
use agent_infrastructure::llm::build_provider;
use agent_test_support::{MockLlmServer, Response};
use async_trait::async_trait;
use serde_json::Value;
use tempfile::TempDir;

struct ApproveAll;

#[async_trait]
impl ApprovalGate for ApproveAll {
    async fn authorize(&self, _: &ApprovalRequest) -> Result<ApprovalDecision, ApprovalError> {
        Ok(ApprovalDecision::Approve)
    }
}

/// A workspace, an agent pointed at a scripted model, and the mock that will
/// report what the agent actually sent.
struct Fixture {
    workspace: TempDir,
    agent: AgentLoop,
    server: MockLlmServer,
}

impl Fixture {
    /// Non-streaming agent: the canned replies here are plain JSON bodies, so
    /// the loop must not ask the provider to stream. The streaming path gets
    /// its own fixture below with SSE replies.
    async fn new(responses: Vec<Response>) -> Self {
        Self::build(responses, false, Arc::new(DefaultPromptBuilder)).await
    }

    async fn new_streaming(responses: Vec<Response>) -> Self {
        Self::build(responses, true, Arc::new(DefaultPromptBuilder)).await
    }

    async fn with_prompt_builder(responses: Vec<Response>, prompt: Arc<dyn PromptBuilder>) -> Self {
        Self::build(responses, false, prompt).await
    }

    async fn with_config(responses: Vec<Response>, config: AgentLoopConfig) -> Self {
        Self::assemble(responses, Arc::new(DefaultPromptBuilder), config).await
    }

    async fn build(responses: Vec<Response>, stream: bool, prompt: Arc<dyn PromptBuilder>) -> Self {
        Self::assemble(
            responses,
            prompt,
            AgentLoopConfig {
                max_iterations: 5,
                stream,
                ..AgentLoopConfig::default()
            },
        )
        .await
    }

    async fn assemble(
        responses: Vec<Response>,
        prompt: Arc<dyn PromptBuilder>,
        config: AgentLoopConfig,
    ) -> Self {
        let workspace = tempfile::tempdir().unwrap();
        let server = MockLlmServer::start(responses).await;
        let root = Arc::new(WorkspaceRoot::new(workspace.path().to_path_buf()).unwrap());

        let settings = LlmSettings {
            providers: vec![ProviderSettings {
                id: ProviderId::new("local"),
                kind: ProviderKind::OpenAiCompatible,
                base_url: server.base_url().to_string(),
                api_key: None,
                model: ModelId::new("mock"),
                max_tokens_field: "max_tokens".into(),
            }],
            default_provider: ProviderId::new("local"),
            router: RouterKind::Static,
            request_timeout: Duration::from_secs(5),
            max_retries: 2,
        };

        let file_system = Arc::new(LocalFileSystem::new(root.clone(), 1024 * 1024).unwrap());
        let tools = Arc::new(
            ToolRegistry::new()
                .with(Arc::new(ReadFileTool::new(
                    file_system.clone(),
                    root.clone(),
                )))
                .with(Arc::new(WriteFileTool::new(file_system, root.clone()))),
        );

        let agent = AgentLoop::new(
            AgentDependencies {
                llm: build_provider(&settings).unwrap(),
                tools,
                approval: Arc::new(ApproveAll),
                events: Arc::new(NullEventSink),
                context: Arc::new(WorkspaceContextProvider::new(root)),
                prompt,
            },
            config,
        );

        Self {
            workspace,
            agent,
            server,
        }
    }

    fn write(&self, relative: &str, contents: &str) {
        let path = self.workspace.path().join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }

    fn read(&self, relative: &str) -> Option<String> {
        std::fs::read_to_string(self.workspace.path().join(relative)).ok()
    }
}

/// The `tool` message the agent sent back for a given call id.
fn tool_result(request: &Value, call_id: &str) -> String {
    request["messages"]
        .as_array()
        .expect("messages")
        .iter()
        .find(|message| message["role"] == "tool" && message["tool_call_id"] == call_id)
        .unwrap_or_else(|| panic!("no tool result for {call_id} in {request}"))["content"]
        .as_str()
        .expect("string content")
        .to_string()
}

#[tokio::test]
async fn the_agent_reads_a_real_file_over_http_and_answers() {
    let fixture = Fixture::new(vec![
        Response::tool_call("call_1", "read_file", r#"{"path":"hello.txt"}"#),
        Response::assistant_text("The file greets you in Japanese."),
    ])
    .await;

    fixture.write("hello.txt", "konnichiwa\n");
    // Picked up by the context provider and injected into the system prompt.
    fixture.write("AGENTS.md", "Always answer in one sentence.");

    let mut session = Session::new("e2e-1");
    let outcome = fixture
        .agent
        .run(&mut session, "what is in hello.txt?")
        .await
        .unwrap();

    assert_eq!(outcome.final_text, "The file greets you in Japanese.");
    assert_eq!(outcome.reason, FinishReason::Completed);
    assert_eq!(outcome.iterations, 2);
    assert_eq!(
        outcome.usage.total(),
        300,
        "usage accumulates across both round-trips"
    );

    let requests = fixture.server.json_requests().await;
    assert_eq!(requests.len(), 2);

    let system = requests[0]["messages"][0]["content"].as_str().unwrap();
    assert!(
        system.contains("Always answer in one sentence."),
        "AGENTS.md reaches the model"
    );
    assert!(
        system.contains("hello.txt"),
        "the workspace overview reaches the model"
    );

    assert!(tool_result(&requests[1], "call_1").contains("konnichiwa"));
}

#[tokio::test]
async fn the_agent_writes_a_real_file_through_the_sandbox() {
    let fixture = Fixture::new(vec![
        // `r##` because the payload itself contains the `"#` sequence.
        Response::tool_call(
            "call_1",
            "write_file",
            r##"{"path":"docs/notes.md","content":"# Notes\n"}"##,
        ),
        Response::assistant_text("Created docs/notes.md."),
    ])
    .await;

    let mut session = Session::new("e2e-2");
    fixture
        .agent
        .run(&mut session, "create docs/notes.md")
        .await
        .unwrap();

    assert_eq!(
        fixture.read("docs/notes.md").as_deref(),
        Some("# Notes\n"),
        "parent directories are created as needed"
    );
}

#[tokio::test]
async fn a_path_outside_the_workspace_is_refused_and_explained_to_the_model() {
    let fixture = Fixture::new(vec![
        Response::tool_call("call_1", "read_file", r#"{"path":"/etc/passwd"}"#),
        Response::assistant_text("I cannot read files outside the workspace."),
    ])
    .await;

    let mut session = Session::new("e2e-3");
    let outcome = fixture
        .agent
        .run(&mut session, "read /etc/passwd")
        .await
        .unwrap();

    assert_eq!(
        outcome.reason,
        FinishReason::Completed,
        "the refusal is recoverable"
    );

    let requests = fixture.server.json_requests().await;
    let content = tool_result(&requests[1], "call_1");

    assert!(content.contains("escapes the workspace"), "got: {content}");
    assert!(
        !content.contains("root:"),
        "no content from outside the sandbox leaks"
    );
}

#[tokio::test]
async fn a_malformed_tool_argument_is_reported_back_to_the_model() {
    let fixture = Fixture::new(vec![
        // The model forgot the required `content` field.
        Response::tool_call("call_1", "write_file", r#"{"path":"a.txt"}"#),
        Response::assistant_text("Sorry, I omitted the file contents."),
    ])
    .await;

    let mut session = Session::new("e2e-4");
    fixture
        .agent
        .run(&mut session, "write a.txt")
        .await
        .unwrap();

    assert!(
        fixture.read("a.txt").is_none(),
        "nothing is written on invalid input"
    );

    let requests = fixture.server.json_requests().await;
    let content = tool_result(&requests[1], "call_1");
    assert!(
        content.contains("content"),
        "the missing field is named: {content}"
    );
}

#[tokio::test]
async fn a_streamed_conversation_reads_a_file_and_answers() {
    // Turn 1: the model streams a tool call whose JSON arguments arrive split
    // across chunks. Turn 2: it streams the prose answer in fragments.
    let fixture = Fixture::new_streaming(vec![
        Response::sse_tool_call_stream(
            "call_1",
            "read_file",
            &[r#"{"pa"#, r#"th":"he"#, r#"llo.txt"}"#],
        ),
        Response::sse_text_stream(&["The file greets ", "you in Japanese."]),
    ])
    .await;

    fixture.write("hello.txt", "konnichiwa\n");

    let mut session = Session::new("e2e-stream-1");
    let outcome = fixture
        .agent
        .run(&mut session, "what is in hello.txt?")
        .await
        .unwrap();

    // The fragmented arguments were aggregated before the tool ran: the file
    // was actually read and its contents fed back to the model.
    assert_eq!(outcome.final_text, "The file greets you in Japanese.");
    assert_eq!(outcome.reason, FinishReason::Completed);
    assert_eq!(
        outcome.usage.total(),
        300,
        "usage still arrives via the stream's usage chunks"
    );

    let requests = fixture.server.json_requests().await;
    assert_eq!(requests[0]["stream"], serde_json::json!(true));
    assert_eq!(
        requests[0]["stream_options"]["include_usage"],
        serde_json::json!(true)
    );
    assert!(
        tool_result(&requests[1], "call_1").contains("konnichiwa"),
        "the aggregated call executed against the real filesystem"
    );
}

#[tokio::test]
async fn a_streamed_turn_can_still_be_denied_by_the_sandbox() {
    let fixture = Fixture::new_streaming(vec![
        Response::sse_tool_call_stream("call_1", "read_file", &[r#"{"path":"/etc/passwd"}"#]),
        Response::sse_text_stream(&["I cannot read files outside the workspace."]),
    ])
    .await;

    let mut session = Session::new("e2e-stream-2");
    let outcome = fixture
        .agent
        .run(&mut session, "read /etc/passwd")
        .await
        .unwrap();

    assert_eq!(outcome.reason, FinishReason::Completed);
    let requests = fixture.server.json_requests().await;
    let content = tool_result(&requests[1], "call_1");
    assert!(content.contains("escapes the workspace"), "got: {content}");
    assert!(!content.contains("root:"));
}

#[tokio::test]
async fn a_replaced_system_prompt_is_sent_verbatim_and_tools_still_work() {
    const OPERATOR_PROMPT: &str = "You are a terse file bot. Use your tools when asked.";

    let fixture = Fixture::with_prompt_builder(
        vec![
            Response::tool_call("call_1", "read_file", r#"{"path":"hello.txt"}"#),
            Response::assistant_text("It says: secret."),
        ],
        Arc::new(FixedPromptBuilder::new(OPERATOR_PROMPT)),
    )
    .await;

    fixture.write("hello.txt", "secret");
    // Would be folded into the *default* prompt; a replaced prompt must not
    // carry it.
    fixture.write("AGENTS.md", "MUST-NOT-APPEAR");

    let mut session = Session::new("e2e-prompt-1");
    let outcome = fixture
        .agent
        .run(&mut session, "what is in hello.txt?")
        .await
        .unwrap();

    assert_eq!(outcome.final_text, "It says: secret.");

    let requests = fixture.server.json_requests().await;

    // The operator's prompt went out verbatim - no default sections, no
    // project instruction file.
    assert_eq!(
        requests[0]["messages"][0]["role"],
        serde_json::json!("system")
    );
    assert_eq!(
        requests[0]["messages"][0]["content"],
        serde_json::json!(OPERATOR_PROMPT)
    );
    assert!(
        !requests[0].to_string().contains("MUST-NOT-APPEAR"),
        "the project instruction file belongs to the default prompt only"
    );

    // Tool definitions travel independently of the prompt, so replacing the
    // prompt must not break tool calling.
    assert_eq!(
        requests[0]["tools"][0]["function"]["name"],
        serde_json::json!("read_file")
    );
    assert!(
        tool_result(&requests[1], "call_1").contains("secret"),
        "the tool actually executed against the real filesystem"
    );
}

/// What a compaction actually puts on the wire.
///
/// Every other test of this feature stops at the port. This one is the only
/// place the summarisation request is serialised by a real provider and read
/// back as JSON - which is where the claim "this request works on every
/// backend" either holds or does not. It is also the request that runs when
/// the session is already in trouble, so it is the worst one to get wrong.
#[tokio::test]
async fn a_compaction_reaches_the_provider_as_a_toolless_single_message_request() {
    let fixture = Fixture::with_config(
        vec![
            Response::assistant_text("RECORD OF THE SESSION"),
            Response::assistant_text("carrying on"),
        ],
        AgentLoopConfig {
            max_iterations: 3,
            // Small enough that the seeded history below overflows it.
            max_history_bytes: 4 * 1024,
            compact: true,
            compact_keep_recent: 2,
            // The canned replies are plain JSON. Note that this only affects
            // the ordinary turn: a summary is never streamed, since nobody is
            // watching it being written.
            stream: false,
            ..AgentLoopConfig::default()
        },
    )
    .await;

    let mut session = Session::new("compaction-e2e");
    session
        .conversation
        .push(Message::user("write a parser, and keep it dependency-free"));
    for turn in 0..4 {
        session
            .conversation
            .push(Message::assistant_text("x".repeat(2_000)));
        session
            .conversation
            .push(Message::user(format!("note {turn}")));
    }

    let outcome = fixture.agent.run(&mut session, "carry on").await.unwrap();
    assert_eq!(outcome.final_text, "carrying on");

    let requests = fixture.server.json_requests().await;
    assert_eq!(requests.len(), 2, "one summary, then the turn itself");

    let summary = &requests[0];
    let messages = summary["messages"].as_array().expect("messages");
    assert_eq!(
        messages.len(),
        2,
        "a system prompt and exactly one user message: {summary}"
    );
    assert_eq!(messages[0]["role"], "system");
    assert!(
        messages[0]["content"]
            .as_str()
            .unwrap()
            .contains("Every user message"),
        "the summariser instructions must reach the model: {summary}"
    );
    assert_eq!(messages[1]["role"], "user");
    assert!(
        messages[1]["content"]
            .as_str()
            .unwrap()
            .contains("write a parser, and keep it dependency-free"),
        "what the user asked for must be in the transcript: {summary}"
    );
    assert!(
        summary.get("tools").is_none(),
        "a summary request must not carry tool definitions: {summary}"
    );
    assert!(
        !messages
            .iter()
            .any(|message| message.get("tool_calls").is_some()),
        "no tool_call blocks may ride along on a toolless request: {summary}"
    );

    // ...and the turn that follows must be built on the summary.
    let turn = &requests[1];
    let first = turn["messages"][1]["content"].as_str().unwrap();
    assert!(
        first.contains("RECORD OF THE SESSION"),
        "the model's next turn must open with the summary: {first}"
    );
    assert!(
        turn["tools"]
            .as_array()
            .is_some_and(|tools| !tools.is_empty()),
        "the ordinary turn still advertises tools"
    );
}
