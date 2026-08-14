//! Tool dispatch.
//!
//! Extracted from the loop because it owns a different concern with its own
//! invariants, and because those invariants are worth stating in one place:
//!
//! * **Every call is answered.** Providers reject a follow-up request that
//!   leaves a `tool_call` without its matching `tool_result`, so an unknown
//!   tool, invalid arguments, a denied approval and a panicking implementation
//!   must all produce a result rather than an error.
//! * **Order is preserved.** Results come back in the order the model asked,
//!   regardless of which ones ran concurrently.
//! * **Only read-only calls run concurrently.** Two writes in one turn may well
//!   touch the same file, so they run sequentially in the requested order.

use std::sync::Arc;
use std::time::Instant;

use agent_domain::model::tool::{ToolCall, ToolDefinition, ToolResult, ToolSafety};
use agent_domain::ports::approval::{ApprovalDecision, ApprovalGate, ApprovalRequest};
use agent_domain::ports::events::{AgentEvent, EventSink};
use agent_domain::ports::tool::Tool;
use agent_domain::text;
use futures::future::join_all;

use crate::tools::registry::ToolRegistry;

/// Longest rendering of a call shown to a human at approval time.
const MAX_SUMMARY_BYTES: usize = 160;
/// Longest string argument shown inside that summary.
const MAX_ARGUMENT_BYTES: usize = 60;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DispatchConfig {
    /// Tool output longer than this is truncated before entering the history.
    /// One `read_file` on a large file must not evict the conversation.
    pub max_tool_output_bytes: usize,
    pub parallel_read_only_tools: bool,
}

pub struct ToolDispatcher {
    tools: Arc<ToolRegistry>,
    approval: Arc<dyn ApprovalGate>,
    events: Arc<dyn EventSink>,
    config: DispatchConfig,
}

/// What the dispatcher decided to do with one call.
enum Plan {
    Run {
        tool: Arc<dyn Tool>,
        safety: ToolSafety,
    },
    /// Resolved without running anything (unknown tool, denial, no approval).
    Short(ToolResult),
}

impl ToolDispatcher {
    pub fn new(
        tools: Arc<ToolRegistry>,
        approval: Arc<dyn ApprovalGate>,
        events: Arc<dyn EventSink>,
        config: DispatchConfig,
    ) -> Self {
        Self {
            tools,
            approval,
            events,
            config,
        }
    }

    pub fn definitions(&self) -> Vec<ToolDefinition> {
        self.tools.definitions()
    }

    /// Produces exactly one [`ToolResult`] per call, in call order.
    pub async fn dispatch(&self, calls: &[ToolCall]) -> Vec<ToolResult> {
        // Phase 1 - resolve and authorise, sequentially: the approval gate may
        // be an interactive prompt, and two prompts at once are unreadable.
        let mut plans = Vec::with_capacity(calls.len());
        for call in calls {
            plans.push(self.plan(call).await);
        }

        // Phase 2 - split by safety, then run.
        let mut results: Vec<Option<ToolResult>> = vec![None; calls.len()];
        let mut concurrent = Vec::new();
        let mut sequential = Vec::new();

        for (index, plan) in plans.into_iter().enumerate() {
            match plan {
                Plan::Short(result) => results[index] = Some(result),
                Plan::Run { tool, safety } => {
                    if self.config.parallel_read_only_tools && safety.is_read_only() {
                        concurrent.push((index, tool, safety));
                    } else {
                        sequential.push((index, tool, safety));
                    }
                }
            }
        }

        if !concurrent.is_empty() {
            let running = concurrent
                .into_iter()
                .map(|(index, tool, safety)| async move {
                    (index, self.invoke(tool, &calls[index], safety).await)
                });
            for (index, result) in join_all(running).await {
                results[index] = Some(result);
            }
        }

        for (index, tool, safety) in sequential {
            results[index] = Some(self.invoke(tool, &calls[index], safety).await);
        }

        results
            .into_iter()
            .enumerate()
            .map(|(index, result)| {
                result.unwrap_or_else(|| {
                    // Unreachable: every index is filled above. Answering anyway
                    // keeps "one result per call" true even if that changes.
                    ToolResult::error(&calls[index], "tool produced no result")
                })
            })
            .collect()
    }

    async fn plan(&self, call: &ToolCall) -> Plan {
        let Some(tool) = self.tools.get(&call.name) else {
            return Plan::Short(ToolResult::error(
                call,
                format!(
                    "Unknown tool `{}`. Available tools: {}. Use one of these instead.",
                    call.name,
                    self.tools.names().join(", ")
                ),
            ));
        };

        let safety = tool.definition().safety;
        let request = ApprovalRequest {
            call: call.clone(),
            safety,
            summary: summarize(call),
        };

        match self.approval.authorize(&request).await {
            Ok(ApprovalDecision::Approve) => Plan::Run { tool, safety },
            Ok(ApprovalDecision::Deny { reason }) => {
                self.events.emit(AgentEvent::ToolCallDenied {
                    name: call.name.clone(),
                    reason: reason.clone(),
                });
                Plan::Short(ToolResult::error(
                    call,
                    format!(
                        "The user declined this tool call: {reason}. \
                         Do not retry it - propose a different approach or ask how to proceed."
                    ),
                ))
            }
            Err(error) => Plan::Short(ToolResult::error(
                call,
                format!("Could not obtain approval for this call: {error}"),
            )),
        }
    }

    async fn invoke(&self, tool: Arc<dyn Tool>, call: &ToolCall, safety: ToolSafety) -> ToolResult {
        self.events.emit(AgentEvent::ToolCallStarted {
            call: call.clone(),
            safety,
        });
        let started = Instant::now();
        let outcome = tool.execute(call.arguments.clone()).await;
        let duration = started.elapsed();

        let (result, summary) = match outcome {
            Ok(outcome) => {
                let (content, truncated) =
                    text::truncate_owned(&outcome.content, self.config.max_tool_output_bytes);
                let summary = outcome
                    .summary
                    .unwrap_or_else(|| text::first_line(&content, 160));
                let content = if truncated {
                    format!(
                        "{content}\n\n[output truncated at {} bytes - narrow the request to see more]",
                        self.config.max_tool_output_bytes
                    )
                } else {
                    content
                };
                (ToolResult::ok(call, content), summary)
            }
            Err(error) => {
                let message = error.to_string();
                (ToolResult::error(call, message.clone()), message)
            }
        };

        self.events.emit(AgentEvent::ToolCallFinished {
            name: call.name.clone(),
            call_id: call.id.clone(),
            is_error: result.is_error,
            summary,
            duration,
        });
        result
    }
}

/// Compact one-line rendering of a call, shown to the human at approval time.
fn summarize(call: &ToolCall) -> String {
    let arguments = match &call.arguments {
        serde_json::Value::Object(map) => map
            .iter()
            .map(|(key, value)| format!("{key}={}", compact(value)))
            .collect::<Vec<_>>()
            .join(", "),
        other => compact(other),
    };
    format!(
        "{}({})",
        call.name,
        text::truncate(&arguments, MAX_SUMMARY_BYTES)
    )
}

fn compact(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(raw) => {
            let (short, truncated) = text::truncate_owned(raw, MAX_ARGUMENT_BYTES);
            let short = short.replace('\n', "\\n");
            if truncated {
                format!("\"{short}...\"")
            } else {
                format!("\"{short}\"")
            }
        }
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_domain::model::tool::{ToolCallId, ToolName};
    use serde_json::json;

    fn call(arguments: serde_json::Value) -> ToolCall {
        ToolCall::new(
            ToolCallId::new("c1"),
            ToolName::new("write_file").unwrap(),
            arguments,
        )
    }

    #[test]
    fn summarises_object_arguments() {
        let summary = summarize(&call(json!({"path": "a.rs", "replace_all": true})));
        assert!(summary.starts_with("write_file("));
        assert!(summary.contains("path=\"a.rs\""));
        assert!(summary.contains("replace_all=true"));
    }

    #[test]
    fn summaries_stay_on_one_line_and_within_budget() {
        let summary = summarize(&call(json!({"content": format!("x\n{}", "y".repeat(500))})));
        assert!(!summary.contains('\n'), "newlines are escaped: {summary}");
        assert!(summary.len() < MAX_SUMMARY_BYTES + 32);
        assert!(
            summary.contains("..."),
            "the elision is visible to the user"
        );
    }

    #[test]
    fn handles_non_object_arguments() {
        assert_eq!(summarize(&call(json!("raw"))), "write_file(\"raw\")");
    }
}
