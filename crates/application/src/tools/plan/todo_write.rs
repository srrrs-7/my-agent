use agent_domain::error::ToolError;
use agent_domain::model::plan::{Plan, PlanItem};
use agent_domain::model::tool::{PlannedWrite, ToolDefinition, ToolName, ToolOutcome, ToolSafety};
use agent_domain::ports::tool::Tool;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use super::render_checklist;
use crate::tools::util::parse_arguments;

/// Writes the model's work plan.
///
/// Stateless: the plan lives in the conversation as this tool's arguments, and
/// the answer is only the validated plan read back - see the module docs.
/// `ReadOnly` because nothing outside the conversation changes, which is also
/// why updating the plan never stops for approval.
#[derive(Debug, Clone, Copy, Default)]
pub struct TodoWriteTool;

#[derive(Debug, Deserialize)]
struct Input {
    todos: Vec<PlanItem>,
}

impl TodoWriteTool {
    pub fn new() -> Self {
        Self
    }

    pub(crate) fn name() -> ToolName {
        ToolName::new("todo_write").expect("static tool name is valid")
    }
}

/// The plan `arguments` describe, or the error the model should see.
///
/// Shared with [`super::latest_plan`] so that "what the tool accepted" and
/// "what counts as the plan" can never disagree.
pub(super) fn parse_plan(name: &ToolName, arguments: Value) -> Result<Plan, ToolError> {
    let input: Input = parse_arguments(name, arguments)?;
    Plan::new(input.todos).map_err(|error| {
        ToolError::invalid_input(
            name,
            format!("{error}. The plan was not changed; send the whole corrected list again."),
        )
    })
}

#[async_trait]
impl Tool for TodoWriteTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: Self::name(),
            description: "Write or replace your work plan: a short checklist of steps, each \
                          pending, in_progress or completed.\n\
                          Use it for tasks of three or more steps. Every call replaces the \
                          whole plan, so always send the full list. Mark a step in_progress \
                          before starting it (only one at a time) and completed as soon as it \
                          is done. Send an empty list to clear the plan. The user sees the \
                          plan; it does not touch the workspace."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "todos": {
                        "type": "array",
                        "description": "The complete plan, in order. Replaces any earlier plan.",
                        "maxItems": agent_domain::model::plan::MAX_PLAN_ITEMS,
                        "items": {
                            "type": "object",
                            "properties": {
                                "content": {
                                    "type": "string",
                                    "description": "One line describing the step.",
                                    "maxLength": agent_domain::model::plan::MAX_PLAN_ITEM_BYTES
                                },
                                "status": {
                                    "type": "string",
                                    "enum": ["pending", "in_progress", "completed"]
                                }
                            },
                            "required": ["content", "status"],
                            "additionalProperties": false
                        }
                    }
                },
                "required": ["todos"],
                "additionalProperties": false
            }),
            safety: ToolSafety::ReadOnly,
        }
    }

    async fn preview(&self, _arguments: &Value) -> Result<Vec<PlannedWrite>, ToolError> {
        // The plan lives in the conversation, not in a file.
        Ok(Vec::new())
    }

    async fn execute(&self, arguments: Value) -> Result<ToolOutcome, ToolError> {
        let plan = parse_plan(&Self::name(), arguments)?;

        if plan.is_empty() {
            return Ok(ToolOutcome::new("The plan is cleared.").with_summary("plan cleared"));
        }

        let done = plan.completed();
        let total = plan.items().len();
        Ok(ToolOutcome::new(format!(
            "Plan updated ({done}/{total} completed):\n{}",
            render_checklist(&plan)
        ))
        .with_summary(format!("{done}/{total} completed")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(arguments: Value) -> Result<ToolOutcome, ToolError> {
        futures::executor::block_on(TodoWriteTool::new().execute(arguments))
    }

    fn error_text(arguments: Value) -> String {
        match run(arguments) {
            Err(error @ ToolError::InvalidInput { .. }) => error.to_string(),
            other => panic!("expected an invalid-input error, got {other:?}"),
        }
    }

    #[test]
    fn a_valid_plan_is_read_back_as_a_checklist() {
        let outcome = run(json!({"todos": [
            {"content": "read the parser", "status": "completed"},
            {"content": "handle CRLF", "status": "in_progress"},
            {"content": "run the tests", "status": "pending"}
        ]}))
        .unwrap();

        assert_eq!(
            outcome.content,
            "Plan updated (1/3 completed):\n[x] read the parser\n[>] handle CRLF\n[ ] run the tests\n"
        );
        assert_eq!(outcome.summary.as_deref(), Some("1/3 completed"));
    }

    #[test]
    fn an_empty_list_is_acknowledged_as_clearing_the_plan() {
        let outcome = run(json!({"todos": []})).unwrap();
        assert!(outcome.content.contains("cleared"), "{}", outcome.content);
    }

    #[test]
    fn two_steps_in_progress_are_refused_with_what_to_change() {
        let message = error_text(json!({"todos": [
            {"content": "a", "status": "in_progress"},
            {"content": "b", "status": "in_progress"}
        ]}));
        assert!(message.contains("items 1, 2"), "{message}");
        assert!(message.contains("only one step"), "{message}");
        assert!(
            message.contains("send the whole corrected list"),
            "{message}"
        );
    }

    #[test]
    fn a_multi_line_item_is_refused_with_its_position() {
        let message = error_text(json!({"todos": [
            {"content": "fine", "status": "pending"},
            {"content": "two\nlines", "status": "pending"}
        ]}));
        assert!(message.contains("item 2"), "{message}");
        assert!(message.contains("single line"), "{message}");
    }

    #[test]
    fn an_unknown_status_is_refused_through_the_schema_message() {
        let message = error_text(json!({"todos": [
            {"content": "a", "status": "blocked"}
        ]}));
        assert!(message.contains("blocked"), "{message}");
        assert!(message.contains("schema"), "{message}");
    }

    #[test]
    fn a_missing_list_is_refused_rather_than_treated_as_clearing() {
        let message = error_text(json!({}));
        assert!(message.contains("todos"), "{message}");
    }

    #[test]
    fn the_tool_is_read_only_so_it_never_waits_for_approval() {
        assert_eq!(
            TodoWriteTool::new().definition().safety,
            ToolSafety::ReadOnly
        );
    }
}
