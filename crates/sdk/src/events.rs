//! The published JSON shapes of what the agent does: one [`EventDto`] per
//! [`AgentEvent`], and the [`ResultDto`] a turn ends with.
//!
//! This is the stable view of an event. [`AgentEvent`] itself is a domain
//! type whose fields move with the domain; an [`EventSink`](crate::EventSink)
//! that wants something to keep, compare or send on converts each event with
//! `EventDto::from(&event)` and serialises that. `agent run --output-format
//! json|stream-json` prints exactly these shapes, each wrapped in
//! [`Versioned`].
//!
//! They are their own types rather than `Serialize` on [`AgentEvent`] or any
//! other domain type: a field renamed in the domain must not silently change
//! what a script parses. Every mapping below is an exhaustive `match`, so a
//! new event variant does not compile until someone decides what it looks
//! like on the wire.
//!
//! Strings from the model and from tools (prose, tool arguments, summaries,
//! plan items) are carried unchanged. JSON escaping keeps them on one line;
//! a writer that prints straight to a terminal escapes the characters a
//! terminal would act on as well, as the `agent` binary does.

use std::time::Duration;

use agent_domain::model::llm::{StopReason, TokenUsage};
use agent_domain::model::tool::ToolSafety;
use agent_domain::ports::events::{AgentEvent, FinishReason};
use serde::Serialize;
use serde_json::Value;

/// The schema version every line carries as `"v"`. Bumped only for a change
/// that would break a reader: a field removed, renamed or retyped. Adding a
/// field or an event type is not one - readers are expected to ignore what
/// they do not know.
pub const SCHEMA_VERSION: u32 = 1;

/// One line of output: the version, then the body's own fields.
#[derive(Debug, Serialize)]
pub struct Versioned<T> {
    pub v: u32,
    #[serde(flatten)]
    pub body: T,
}

impl<T> Versioned<T> {
    pub fn new(body: T) -> Self {
        Self {
            v: SCHEMA_VERSION,
            body,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
pub struct UsageDto {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
}

impl From<&TokenUsage> for UsageDto {
    fn from(usage: &TokenUsage) -> Self {
        Self {
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
            cache_read_tokens: usage.cache_read_tokens,
            cache_write_tokens: usage.cache_write_tokens,
        }
    }
}

/// Why a turn ended, flattened into the object that reports it: `reason`
/// always, `limit` / `stop_reason` / `error` only for the reason they explain.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
#[non_exhaustive]
pub enum ReasonDto {
    Completed,
    MaxIterations {
        limit: u32,
    },
    Stopped {
        stop_reason: String,
    },
    Interrupted,
    /// The turn failed - the provider is unreachable or refused the request,
    /// or the agent could not start. Never an event: only the result has it.
    Error {
        error: String,
    },
}

impl From<&FinishReason> for ReasonDto {
    fn from(reason: &FinishReason) -> Self {
        match reason {
            FinishReason::Completed => Self::Completed,
            FinishReason::MaxIterations { limit } => Self::MaxIterations { limit: *limit },
            FinishReason::Stopped { stop_reason } => Self::Stopped {
                stop_reason: stop_reason.clone(),
            },
            FinishReason::Interrupted => Self::Interrupted,
        }
    }
}

/// The last line of `stream-json`, and the only one of `json`.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", rename = "result")]
pub struct ResultDto {
    #[serde(flatten)]
    pub reason: ReasonDto,
    /// The answer. Empty when the turn produced none; partial when it was
    /// interrupted or failed after the model had already said something.
    pub final_text: String,
    /// `null` only when the agent failed before a session existed.
    pub session_id: Option<String>,
    pub iterations: u32,
    pub usage: UsageDto,
    /// The process's exit code, so a reader holding only stdout (a log file)
    /// still knows it: 0 completed, 1 error, 2 out of iterations or stopped by
    /// the provider, 130 interrupted.
    pub exit_code: u8,
}

/// One `stream-json` line per [`AgentEvent`].
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum EventDto<'a> {
    RunStarted {
        session_id: &'a str,
        provider: &'a str,
        /// `null` when the provider's default model is used.
        model: Option<&'a str>,
    },
    IterationStarted {
        iteration: u32,
        limit: u32,
    },
    HistoryTrimmed {
        dropped_messages: usize,
    },
    HistoryCompacted {
        folded_messages: usize,
        before_bytes: usize,
        after_bytes: usize,
    },
    ModelResponded {
        provider: &'a str,
        model: &'a str,
        /// `end_turn`, `tool_use`, `max_tokens`, `stop_sequence`, or the
        /// provider's own value as it sent it.
        stop_reason: &'a str,
        usage: UsageDto,
        latency_ms: u64,
    },
    AssistantDelta {
        text: &'a str,
    },
    AssistantMessage {
        text: &'a str,
    },
    ToolCallStarted {
        call_id: &'a str,
        name: &'a str,
        /// The arguments exactly as the model wrote them.
        arguments: &'a Value,
        /// `read_only`, `mutating`, `network` or `destructive`.
        safety: &'static str,
    },
    ToolCallFinished {
        call_id: &'a str,
        name: &'a str,
        is_error: bool,
        summary: &'a str,
        duration_ms: u64,
    },
    ToolCallDenied {
        name: &'a str,
        reason: &'a str,
    },
    DependenciesChanged {
        name: &'a str,
        files: Vec<DependencyFileDto>,
    },
    DependencyCheckIncomplete {
        name: &'a str,
        limit: usize,
    },
    PlanUpdated {
        /// The whole plan; empty when the model cleared it.
        items: Vec<PlanItemDto<'a>>,
    },
    RunFinished {
        #[serde(flatten)]
        reason: ReasonDto,
        iterations: u32,
        usage: UsageDto,
    },
    /// An event of the sub-agent that the `spawn_agent` call
    /// `parent_call_id` started, in the same shape as the parent's own (minus
    /// `v`, which the line already carries). Its `assistant_message` is the
    /// sub-agent's answer to the parent, not the turn's answer: that is in
    /// the result.
    Nested {
        parent_call_id: &'a str,
        event: Box<EventDto<'a>>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DependencyFileDto {
    pub path: String,
    /// `manifest`, `lockfile` or `config`.
    pub kind: &'static str,
    /// `created`, `modified` or `deleted`.
    pub change: &'static str,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PlanItemDto<'a> {
    pub content: &'a str,
    /// `pending`, `in_progress` or `completed`.
    pub status: &'static str,
}

impl<'a> From<&'a AgentEvent> for EventDto<'a> {
    fn from(event: &'a AgentEvent) -> Self {
        match event {
            AgentEvent::RunStarted {
                session_id,
                provider,
                model,
            } => Self::RunStarted {
                session_id,
                provider: provider.as_str(),
                model: model.as_ref().map(|model| model.as_str()),
            },
            AgentEvent::IterationStarted { iteration, limit } => Self::IterationStarted {
                iteration: *iteration,
                limit: *limit,
            },
            AgentEvent::HistoryTrimmed { dropped_messages } => Self::HistoryTrimmed {
                dropped_messages: *dropped_messages,
            },
            AgentEvent::HistoryCompacted {
                folded_messages,
                before_bytes,
                after_bytes,
            } => Self::HistoryCompacted {
                folded_messages: *folded_messages,
                before_bytes: *before_bytes,
                after_bytes: *after_bytes,
            },
            AgentEvent::ModelResponded {
                provider,
                model,
                stop_reason,
                usage,
                latency,
            } => Self::ModelResponded {
                provider: provider.as_str(),
                model: model.as_str(),
                stop_reason: stop_reason_name(stop_reason),
                usage: usage.into(),
                latency_ms: millis(*latency),
            },
            AgentEvent::AssistantDelta { text } => Self::AssistantDelta { text },
            AgentEvent::AssistantMessage { text } => Self::AssistantMessage { text },
            AgentEvent::ToolCallStarted { call, safety } => Self::ToolCallStarted {
                call_id: call.id.as_str(),
                name: call.name.as_str(),
                arguments: &call.arguments,
                safety: safety_name(*safety),
            },
            AgentEvent::ToolCallFinished {
                name,
                call_id,
                is_error,
                summary,
                duration,
            } => Self::ToolCallFinished {
                call_id: call_id.as_str(),
                name: name.as_str(),
                is_error: *is_error,
                summary,
                duration_ms: millis(*duration),
            },
            AgentEvent::ToolCallDenied { name, reason } => Self::ToolCallDenied {
                name: name.as_str(),
                reason,
            },
            AgentEvent::DependenciesChanged { name, files } => Self::DependenciesChanged {
                name: name.as_str(),
                files: files
                    .iter()
                    .map(|file| DependencyFileDto {
                        path: file.path.to_string(),
                        kind: file.kind.label(),
                        change: file.change.label(),
                    })
                    .collect(),
            },
            AgentEvent::DependencyCheckIncomplete { name, limit } => {
                Self::DependencyCheckIncomplete {
                    name: name.as_str(),
                    limit: *limit,
                }
            }
            AgentEvent::PlanUpdated { plan } => Self::PlanUpdated {
                items: plan
                    .items()
                    .iter()
                    .map(|item| PlanItemDto {
                        content: &item.content,
                        status: item.status.as_str(),
                    })
                    .collect(),
            },
            AgentEvent::RunFinished {
                reason,
                iterations,
                usage,
            } => Self::RunFinished {
                reason: reason.into(),
                iterations: *iterations,
                usage: usage.into(),
            },
            AgentEvent::Nested {
                parent_call_id,
                event,
            } => Self::Nested {
                parent_call_id: parent_call_id.as_str(),
                event: Box::new(Self::from(event.as_ref())),
            },
        }
    }
}

fn stop_reason_name(reason: &StopReason) -> &str {
    match reason {
        StopReason::EndTurn => "end_turn",
        StopReason::ToolUse => "tool_use",
        StopReason::MaxTokens => "max_tokens",
        StopReason::StopSequence => "stop_sequence",
        StopReason::Other(raw) => raw,
    }
}

fn safety_name(safety: ToolSafety) -> &'static str {
    match safety {
        ToolSafety::ReadOnly => "read_only",
        ToolSafety::Mutating => "mutating",
        ToolSafety::Network => "network",
        ToolSafety::Destructive => "destructive",
    }
}

/// Saturates rather than wraps: a duration past `u64::MAX` milliseconds is
/// not a value anyone will see, and a wrong small number would be worse.
fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_domain::model::dependency::{DependencyFileChange, FileChange, ManifestKind};
    use agent_domain::model::llm::{ModelId, ProviderId};
    use agent_domain::model::plan::{Plan, PlanItem, PlanStatus};
    use agent_domain::model::tool::{ToolCall, ToolCallId, ToolName};
    use agent_domain::model::workspace::WorkspacePath;
    use serde_json::json;

    fn usage() -> TokenUsage {
        TokenUsage {
            input_tokens: 10,
            output_tokens: 5,
            cache_read_tokens: 3,
            cache_write_tokens: 2,
        }
    }

    fn usage_json() -> Value {
        json!({
            "input_tokens": 10,
            "output_tokens": 5,
            "cache_read_tokens": 3,
            "cache_write_tokens": 2,
        })
    }

    fn line(event: &AgentEvent) -> Value {
        serde_json::to_value(Versioned::new(EventDto::from(event))).unwrap()
    }

    /// One instance of every variant, with what it must look like. The match
    /// in `covers` has no wildcard: a new variant fails to compile here until
    /// it has an expected line, the same way the mapping itself does.
    fn every_event() -> Vec<(AgentEvent, Value)> {
        let tool = ToolName::new("read_file").unwrap();
        let plan = Plan::new(vec![
            PlanItem::new("read", PlanStatus::Completed),
            PlanItem::new("write", PlanStatus::InProgress),
        ])
        .unwrap();
        vec![
            (
                AgentEvent::RunStarted {
                    session_id: "s-1".into(),
                    provider: ProviderId::new("local"),
                    model: None,
                },
                json!({"v": 1, "type": "run_started", "session_id": "s-1",
                       "provider": "local", "model": null}),
            ),
            (
                AgentEvent::IterationStarted {
                    iteration: 2,
                    limit: 25,
                },
                json!({"v": 1, "type": "iteration_started", "iteration": 2, "limit": 25}),
            ),
            (
                AgentEvent::HistoryTrimmed {
                    dropped_messages: 4,
                },
                json!({"v": 1, "type": "history_trimmed", "dropped_messages": 4}),
            ),
            (
                AgentEvent::HistoryCompacted {
                    folded_messages: 6,
                    before_bytes: 9000,
                    after_bytes: 1000,
                },
                json!({"v": 1, "type": "history_compacted", "folded_messages": 6,
                       "before_bytes": 9000, "after_bytes": 1000}),
            ),
            (
                AgentEvent::ModelResponded {
                    provider: ProviderId::new("cloud"),
                    model: ModelId::new("m-1"),
                    stop_reason: StopReason::Other("content_filter".into()),
                    usage: usage(),
                    latency: Duration::from_millis(1500),
                },
                json!({"v": 1, "type": "model_responded", "provider": "cloud", "model": "m-1",
                       "stop_reason": "content_filter", "usage": usage_json(),
                       "latency_ms": 1500}),
            ),
            (
                AgentEvent::AssistantDelta { text: "Hel".into() },
                json!({"v": 1, "type": "assistant_delta", "text": "Hel"}),
            ),
            (
                AgentEvent::AssistantMessage {
                    text: "Hello".into(),
                },
                json!({"v": 1, "type": "assistant_message", "text": "Hello"}),
            ),
            (
                AgentEvent::ToolCallStarted {
                    call: ToolCall::new(
                        ToolCallId::new("c1"),
                        tool.clone(),
                        json!({"path": "src/main.rs"}),
                    ),
                    safety: ToolSafety::ReadOnly,
                },
                json!({"v": 1, "type": "tool_call_started", "call_id": "c1",
                       "name": "read_file", "arguments": {"path": "src/main.rs"},
                       "safety": "read_only"}),
            ),
            (
                AgentEvent::ToolCallFinished {
                    name: tool.clone(),
                    call_id: ToolCallId::new("c1"),
                    is_error: true,
                    summary: "not found".into(),
                    duration: Duration::from_millis(7),
                },
                json!({"v": 1, "type": "tool_call_finished", "call_id": "c1",
                       "name": "read_file", "is_error": true, "summary": "not found",
                       "duration_ms": 7}),
            ),
            (
                AgentEvent::ToolCallDenied {
                    name: tool.clone(),
                    reason: "denied by the user".into(),
                },
                json!({"v": 1, "type": "tool_call_denied", "name": "read_file",
                       "reason": "denied by the user"}),
            ),
            (
                AgentEvent::DependenciesChanged {
                    name: tool.clone(),
                    files: vec![DependencyFileChange {
                        path: WorkspacePath::parse("Cargo.lock").unwrap(),
                        kind: ManifestKind::CargoLock,
                        change: FileChange::Modified,
                    }],
                },
                json!({"v": 1, "type": "dependencies_changed", "name": "read_file",
                       "files": [{"path": "Cargo.lock", "kind": "lockfile",
                                  "change": "modified"}]}),
            ),
            (
                AgentEvent::DependencyCheckIncomplete {
                    name: tool,
                    limit: 5000,
                },
                json!({"v": 1, "type": "dependency_check_incomplete", "name": "read_file",
                       "limit": 5000}),
            ),
            (
                AgentEvent::PlanUpdated { plan },
                json!({"v": 1, "type": "plan_updated", "items": [
                    {"content": "read", "status": "completed"},
                    {"content": "write", "status": "in_progress"},
                ]}),
            ),
            (
                AgentEvent::RunFinished {
                    reason: FinishReason::MaxIterations { limit: 25 },
                    iterations: 25,
                    usage: usage(),
                },
                json!({"v": 1, "type": "run_finished", "reason": "max_iterations",
                       "limit": 25, "iterations": 25, "usage": usage_json()}),
            ),
            (
                AgentEvent::Nested {
                    parent_call_id: ToolCallId::new("spawn-1"),
                    event: Box::new(AgentEvent::ToolCallStarted {
                        call: ToolCall::new(
                            ToolCallId::new("c9"),
                            ToolName::new("glob").unwrap(),
                            json!({"pattern": "*.rs"}),
                        ),
                        safety: ToolSafety::ReadOnly,
                    }),
                },
                json!({"v": 1, "type": "nested", "parent_call_id": "spawn-1",
                       "event": {"type": "tool_call_started", "call_id": "c9",
                                 "name": "glob", "arguments": {"pattern": "*.rs"},
                                 "safety": "read_only"}}),
            ),
        ]
    }

    /// Which variant an event is, by an exhaustive match - the compile-time
    /// half of [`every_event_maps_to_its_documented_line`].
    fn covers(event: &AgentEvent) -> usize {
        match event {
            AgentEvent::RunStarted { .. } => 0,
            AgentEvent::IterationStarted { .. } => 1,
            AgentEvent::HistoryTrimmed { .. } => 2,
            AgentEvent::HistoryCompacted { .. } => 3,
            AgentEvent::ModelResponded { .. } => 4,
            AgentEvent::AssistantDelta { .. } => 5,
            AgentEvent::AssistantMessage { .. } => 6,
            AgentEvent::ToolCallStarted { .. } => 7,
            AgentEvent::ToolCallFinished { .. } => 8,
            AgentEvent::ToolCallDenied { .. } => 9,
            AgentEvent::DependenciesChanged { .. } => 10,
            AgentEvent::DependencyCheckIncomplete { .. } => 11,
            AgentEvent::PlanUpdated { .. } => 12,
            AgentEvent::RunFinished { .. } => 13,
            AgentEvent::Nested { .. } => 14,
        }
    }

    const VARIANTS: usize = 15;

    #[test]
    fn every_event_maps_to_its_documented_line() {
        let cases = every_event();
        let mut seen = [false; VARIANTS];
        for (event, expected) in &cases {
            seen[covers(event)] = true;
            assert_eq!(&line(event), expected, "for {event:?}");
        }
        assert!(
            seen.iter().all(|&seen| seen),
            "a variant has no expected line: {seen:?}"
        );
    }

    #[test]
    fn every_finish_reason_has_its_own_name() {
        let cases = [
            (FinishReason::Completed, json!({"reason": "completed"})),
            (
                FinishReason::MaxIterations { limit: 3 },
                json!({"reason": "max_iterations", "limit": 3}),
            ),
            (
                FinishReason::Stopped {
                    stop_reason: "max_tokens".into(),
                },
                json!({"reason": "stopped", "stop_reason": "max_tokens"}),
            ),
            (FinishReason::Interrupted, json!({"reason": "interrupted"})),
        ];
        for (reason, expected) in cases {
            assert_eq!(
                serde_json::to_value(ReasonDto::from(&reason)).unwrap(),
                expected
            );
        }
    }

    #[test]
    fn the_known_stop_reasons_have_stable_names() {
        assert_eq!(stop_reason_name(&StopReason::EndTurn), "end_turn");
        assert_eq!(stop_reason_name(&StopReason::ToolUse), "tool_use");
        assert_eq!(stop_reason_name(&StopReason::MaxTokens), "max_tokens");
        assert_eq!(stop_reason_name(&StopReason::StopSequence), "stop_sequence");
        assert_eq!(safety_name(ToolSafety::Mutating), "mutating");
        assert_eq!(safety_name(ToolSafety::Network), "network");
        assert_eq!(safety_name(ToolSafety::Destructive), "destructive");
    }

    #[test]
    fn the_result_carries_the_version_reason_and_exit_code() {
        let result = ResultDto {
            reason: ReasonDto::Error {
                error: "HTTP 500".into(),
            },
            final_text: String::new(),
            session_id: None,
            iterations: 1,
            usage: UsageDto::default(),
            exit_code: 1,
        };
        assert_eq!(
            serde_json::to_value(Versioned::new(result)).unwrap(),
            json!({"v": 1, "type": "result", "reason": "error", "error": "HTTP 500",
                   "final_text": "", "session_id": null, "iterations": 1,
                   "usage": {"input_tokens": 0, "output_tokens": 0,
                             "cache_read_tokens": 0, "cache_write_tokens": 0},
                   "exit_code": 1})
        );
    }
}
