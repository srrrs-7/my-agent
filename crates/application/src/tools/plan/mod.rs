//! The work plan: a tool the model writes it with, and the rule that says which
//! plan is current.
//!
//! ## The conversation is the source of truth
//!
//! The current plan is the arguments of the most recent *successful*
//! `todo_write` call - nothing else. The tool itself keeps no state: it
//! validates, renders, and answers. That is what lets a resumed session get its
//! plan back from the record alone, with no second file to keep in step, and
//! what keeps the tool safe to run in parallel: same-turn results come back in
//! request order, so "the last one wins" is decided by the order the model
//! asked in, not by which call finished first.
//!
//! [`latest_plan`] is that rule written down. The session caches its answer
//! (see [`Session::plan`](crate::agent::Session::plan)) because the history it
//! reads from gets shortened, and a plan folded into a summary is a plan the
//! projection can no longer see.

mod todo_write;

use std::collections::HashMap;
use std::fmt::Write as _;

use agent_domain::model::message::{ContentBlock, Message, Role};
use agent_domain::model::plan::{Plan, PlanStatus};
use serde_json::Value;

pub use todo_write::TodoWriteTool;

use todo_write::parse_plan;

/// The plan the last successful `todo_write` in `messages` wrote, or `None`
/// when there was no such call.
///
/// An empty plan is returned as such - it is the model clearing its plan, which
/// is different from never having written one.
///
/// A call only counts once its result says it succeeded: a refused plan, a
/// denied call or one that timed out never became the plan, and the model was
/// told so.
///
/// A result is matched to its call within the assistant message right before
/// it, never across the whole history. Call ids are only unique within one
/// turn: a server that sends none gets `call_0`, `call_1`, ... from the
/// adapter every turn, so a history-wide lookup by id would pair a refused
/// plan's result with an earlier turn's arguments - or a successful result
/// with arguments that were refused.
pub fn latest_plan(messages: &[Message]) -> Option<Plan> {
    let name = TodoWriteTool::name();

    let mut written: Vec<&Value> = Vec::new();
    let mut turn: HashMap<&str, &Value> = HashMap::new();
    for message in messages {
        if message.role == Role::Assistant {
            turn = message
                .tool_calls()
                .filter(|call| call.name == name)
                .map(|call| (call.id.as_str(), &call.arguments))
                .collect();
            continue;
        }
        for block in &message.content {
            if let ContentBlock::ToolResult(result) = block
                && !result.is_error
                && result.tool_name == name
                && let Some(arguments) = turn.get(result.call_id.as_str())
            {
                written.push(arguments);
            }
        }
    }

    // Newest first, so only the plan that wins is ever parsed.
    written
        .into_iter()
        .rev()
        .find_map(|arguments| parse_plan(&name, arguments.clone()).ok())
}

/// The plan as plain text, one item per line: what the tool answers with and
/// what is pinned into a shortened history, so the model sees one format.
pub fn render_checklist(plan: &Plan) -> String {
    let mut rendered = String::new();
    for item in plan.items() {
        let marker = match item.status {
            PlanStatus::Completed => "[x]",
            PlanStatus::InProgress => "[>]",
            PlanStatus::Pending => "[ ]",
        };
        let _ = writeln!(rendered, "{marker} {}", item.content);
    }
    rendered
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_domain::model::tool::{ToolCall, ToolCallId, ToolName, ToolResult};
    use serde_json::json;

    fn call(id: &str, tool: &str, arguments: Value) -> ToolCall {
        ToolCall::new(ToolCallId::new(id), ToolName::new(tool).unwrap(), arguments)
    }

    fn plan_args(steps: &[(&str, &str)]) -> Value {
        json!({
            "todos": steps
                .iter()
                .map(|(content, status)| json!({"content": content, "status": status}))
                .collect::<Vec<_>>()
        })
    }

    /// One assistant turn with `calls`, answered by `results` in the same order.
    fn turn(calls: Vec<ToolCall>, ok: &[bool]) -> Vec<Message> {
        let results = calls
            .iter()
            .zip(ok)
            .map(|(call, ok)| {
                if *ok {
                    ToolResult::ok(call, "fine")
                } else {
                    ToolResult::error(call, "refused")
                }
            })
            .collect();
        vec![
            Message::assistant(calls.into_iter().map(ContentBlock::ToolCall).collect()),
            Message::tool_results(results),
        ]
    }

    fn contents(plan: &Plan) -> Vec<&str> {
        plan.items().iter().map(|i| i.content.as_str()).collect()
    }

    #[test]
    fn no_todo_write_means_no_plan() {
        let messages = turn(vec![call("c1", "read_file", json!({"path": "a"}))], &[true]);
        assert_eq!(latest_plan(&messages), None);
        assert_eq!(latest_plan(&[]), None);
    }

    #[test]
    fn only_a_successful_call_becomes_the_plan() {
        let mut messages = turn(
            vec![call(
                "c1",
                "todo_write",
                plan_args(&[("first", "in_progress")]),
            )],
            &[true],
        );
        messages.extend(turn(
            vec![call(
                "c2",
                "todo_write",
                plan_args(&[("refused", "pending")]),
            )],
            &[false],
        ));

        let plan = latest_plan(&messages).expect("the first call succeeded");
        assert_eq!(contents(&plan), ["first"]);
    }

    #[test]
    fn the_last_call_of_a_turn_wins() {
        let messages = turn(
            vec![
                call("c1", "todo_write", plan_args(&[("one", "pending")])),
                call("c2", "read_file", json!({"path": "a"})),
                call("c3", "todo_write", plan_args(&[("three", "pending")])),
            ],
            &[true, true, true],
        );
        assert_eq!(contents(&latest_plan(&messages).unwrap()), ["three"]);
    }

    #[test]
    fn a_later_turn_replaces_an_earlier_one_whole() {
        let mut messages = turn(
            vec![call(
                "c1",
                "todo_write",
                plan_args(&[("a", "pending"), ("b", "pending")]),
            )],
            &[true],
        );
        messages.extend(turn(
            vec![call("c2", "todo_write", plan_args(&[("c", "completed")]))],
            &[true],
        ));
        assert_eq!(contents(&latest_plan(&messages).unwrap()), ["c"]);
    }

    #[test]
    fn an_empty_list_clears_the_plan_rather_than_being_ignored() {
        let mut messages = turn(
            vec![call("c1", "todo_write", plan_args(&[("a", "pending")]))],
            &[true],
        );
        messages.extend(turn(
            vec![call("c2", "todo_write", json!({"todos": []}))],
            &[true],
        ));
        let plan = latest_plan(&messages).expect("clearing is a plan too");
        assert!(plan.is_empty());
    }

    #[test]
    fn arguments_wrapped_in_a_json_string_still_count() {
        let wrapped = Value::String(plan_args(&[("wrapped", "pending")]).to_string());
        let messages = turn(vec![call("c1", "todo_write", wrapped)], &[true]);
        assert_eq!(contents(&latest_plan(&messages).unwrap()), ["wrapped"]);
    }

    #[test]
    fn the_checklist_marks_each_status() {
        let plan = parse_plan(
            &TodoWriteTool::name(),
            plan_args(&[
                ("done", "completed"),
                ("doing", "in_progress"),
                ("next", "pending"),
            ]),
        )
        .unwrap();
        assert_eq!(render_checklist(&plan), "[x] done\n[>] doing\n[ ] next\n");
    }

    /// What the OpenAI-compatible adapter produces for a server that sends no
    /// call ids: the same `call_0` in every turn.
    #[test]
    fn a_refused_plan_reusing_an_earlier_id_does_not_replace_the_plan() {
        let mut messages = turn(
            vec![call(
                "call_0",
                "todo_write",
                plan_args(&[("kept", "pending")]),
            )],
            &[true],
        );
        messages.extend(turn(
            vec![call(
                "call_0",
                "todo_write",
                plan_args(&[("refused", "pending")]),
            )],
            &[false],
        ));

        assert_eq!(contents(&latest_plan(&messages).unwrap()), ["kept"]);
    }

    #[test]
    fn a_successful_result_is_paired_with_its_own_turns_arguments() {
        // Turn 1 writes "first"; turn 2 is denied but reuses the id; turn 3
        // reuses it again and succeeds. Each result must read the arguments
        // of the call right before it.
        let mut messages = turn(
            vec![call(
                "call_0",
                "todo_write",
                plan_args(&[("first", "pending")]),
            )],
            &[true],
        );
        messages.extend(turn(
            vec![call(
                "call_0",
                "todo_write",
                plan_args(&[("denied", "pending")]),
            )],
            &[false],
        ));
        messages.extend(turn(
            vec![call(
                "call_0",
                "todo_write",
                plan_args(&[("third", "pending")]),
            )],
            &[true],
        ));
        assert_eq!(contents(&latest_plan(&messages).unwrap()), ["third"]);

        // And a later turn's arguments are never read for an earlier result.
        let mut messages = turn(
            vec![call(
                "call_0",
                "todo_write",
                plan_args(&[("first", "pending")]),
            )],
            &[true],
        );
        messages.push(Message::assistant(vec![ContentBlock::ToolCall(call(
            "call_0",
            "todo_write",
            plan_args(&[("never answered", "pending")]),
        ))]));
        assert_eq!(contents(&latest_plan(&messages).unwrap()), ["first"]);
    }
}
