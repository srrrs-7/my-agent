//! `post_tool_use`: the operator's hooks, run after a tool call and told
//! what it returned.
//!
//! Observation only. The tool's result - output, summary or error - is what
//! reaches the model, byte for byte, whatever the hooks do or fail to do. A
//! hook that wants to change something changes the workspace (a formatter),
//! and the model finds that out the way it finds out any other change.

use std::sync::Arc;

use agent_domain::error::ToolError;
use agent_domain::model::tool::{PlannedWrite, ToolDefinition, ToolOutcome};
use agent_domain::ports::tool::Tool;
use agent_domain::text;
use async_trait::async_trait;
use serde_json::{Value, json};

use super::{Hooks, MAX_POST_OUTPUT_BYTES, encode};

pub struct HookedTool {
    inner: Arc<dyn Tool>,
    hooks: Arc<Hooks>,
}

impl HookedTool {
    pub fn new(inner: Arc<dyn Tool>, hooks: Arc<Hooks>) -> Self {
        Self { inner, hooks }
    }

    /// `inner`, wrapped when a post hook is about it and as it is otherwise:
    /// a tool no hook watches costs nothing.
    pub fn wrap(inner: Arc<dyn Tool>, hooks: &Arc<Hooks>) -> Arc<dyn Tool> {
        if hooks.observes(inner.definition().name.as_str()) {
            Arc::new(Self::new(inner, hooks.clone()))
        } else {
            inner
        }
    }

    /// The document a post hook reads: the pre hook's, plus how the call
    /// went. The output is redacted before it is cut, so a cut can never
    /// leave half a secret that the redactor would no longer recognise.
    fn payload(&self, arguments: &Value, result: &Result<ToolOutcome, ToolError>) -> Value {
        let definition = self.inner.definition();
        let call_id = self.hooks.take_note(&definition.name, arguments);
        let mut payload = self.hooks.tool_payload(
            "post_tool_use",
            &definition.name,
            definition.safety,
            call_id.as_ref(),
            arguments,
        );
        let (is_error, output) = match result {
            Ok(outcome) => (false, outcome.content.clone()),
            Err(error) => (true, error.to_string()),
        };
        let redacted = self.hooks.redactor.redact(&output);
        let output = text::truncate(&redacted, MAX_POST_OUTPUT_BYTES);
        if let Value::Object(map) = &mut payload {
            map.insert("is_error".into(), json!(is_error));
            map.insert("output".into(), json!(output));
        }
        payload
    }
}

#[async_trait]
impl Tool for HookedTool {
    fn definition(&self) -> ToolDefinition {
        self.inner.definition()
    }

    async fn preview(&self, arguments: &Value) -> Result<Vec<PlannedWrite>, ToolError> {
        self.inner.preview(arguments).await
    }

    async fn execute(&self, arguments: Value) -> Result<ToolOutcome, ToolError> {
        let payload_arguments = arguments.clone();
        let result = self.inner.execute(arguments).await;
        let payload = encode(&self.payload(&payload_arguments, &result));

        let name = self.inner.definition().name;
        let matching = self
            .hooks
            .config
            .post_tool_use
            .iter()
            .filter(|hook| hook.matches(name.as_str()));
        for hook in matching {
            let outcome = self
                .hooks
                .runner
                .run(&hook.command, payload.clone(), hook.timeout())
                .await;
            self.hooks
                .warn_on_failure("post_tool_use", &hook.command, outcome);
        }
        result
    }
}
