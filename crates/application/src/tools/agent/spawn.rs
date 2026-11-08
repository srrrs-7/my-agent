//! `spawn_agent`: a read-only sub-agent that investigates and reports back.
//!
//! The point is the parent's context budget. An investigation is a long run
//! of file reads and searches whose outputs are mostly irrelevant once the
//! answer is known, and in the parent's history every one of them stays
//! until compaction or trimming takes it - usually along with something that
//! mattered. Run in a sub-agent, they fill the sub-agent's history instead,
//! and the parent's grows by the summary alone.
//!
//! ## How it is put together
//!
//! The sub-agent is an ordinary [`AgentLoop`], built before this tool and
//! handed to it, over a registry that holds only read-only tools and not this
//! one (see [`ToolRegistry::read_only`]). That construction order is what
//! breaks the loop -> registry -> tool -> loop cycle, and it is also what
//! limits the nesting to one level: the sub-agent has no `spawn_agent` to
//! call. Nothing is counted at run time, so there is no depth setting.
//!
//! The sub-agent shares the parent's approval gate - the same instance, plan
//! mode, hooks and the operator's rules included - so a call is decided the
//! same way whichever agent makes it. It shares the conversation log as
//! well, under its own session id (`"{parent}.{call id}"`, see
//! [`subagent_session_id`]); its history never enters the parent's.
//!
//! ## What reaches the parent
//!
//! Its final answer, clipped to [`SubagentLimits::max_answer_bytes`], as this
//! call's result; the tokens it spent, through [`ToolOutcome::usage`]; and its
//! events, wrapped in [`AgentEvent::Nested`](agent_domain::ports::events::AgentEvent::Nested).
//! A sub-agent that does not finish - out of iterations, stopped by the
//! provider, the provider failing - is a failed call, which the parent's
//! model reads and works around like any other; the parent's turn goes on.
//!
//! Stopping the parent's turn stops the sub-agent: the dispatcher drops the
//! call's future, and the sub-agent's loop goes with it. Its own interrupt is
//! never fired, so nothing in its history is answered on the way out - the
//! history is thrown away, and the record it leaves ends where it stopped.

use std::sync::Arc;

use agent_domain::error::ToolError;
use agent_domain::model::tool::{PlannedWrite, ToolDefinition, ToolName, ToolOutcome, ToolSafety};
use agent_domain::ports::conversation_log::subagent_session_id;
use agent_domain::ports::events::FinishReason;
use agent_domain::ports::tool::Tool;
use agent_domain::text;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::agent::call_scope;
use crate::agent::{AgentLoop, AgentOutcome, Interrupt, Session};
use crate::tools::util::parse_arguments;

/// The tool's name, for whoever has to recognise it (the permission rules,
/// the composition root).
pub const SPAWN_AGENT: &str = "spawn_agent";

/// What bounds a sub-agent on top of its loop's own configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SubagentLimits {
    /// Longest answer handed back to the parent, in bytes. The summary is
    /// what the parent pays for; one that runs to the dispatcher's output
    /// limit has defeated the purpose of delegating.
    pub max_answer_bytes: usize,
}

pub struct SpawnAgentTool {
    child: Arc<AgentLoop>,
    limits: SubagentLimits,
}

#[derive(Debug, Deserialize)]
struct Input {
    task: String,
}

impl SpawnAgentTool {
    /// `child` is the sub-agent's loop: built over the read-only tools,
    /// without this one, and with the parent's approval gate and sink.
    pub fn new(child: Arc<AgentLoop>, limits: SubagentLimits) -> Self {
        Self { child, limits }
    }

    fn name() -> ToolName {
        ToolName::new(SPAWN_AGENT).expect("static tool name is valid")
    }

    /// What the model is told about a sub-agent that did not finish, and
    /// what it should do about it.
    fn unfinished(&self, outcome: &AgentOutcome) -> ToolError {
        let said = if outcome.final_text.trim().is_empty() {
            "It said nothing usable before it stopped.".to_string()
        } else {
            format!(
                "What it had said when it stopped:\n\n{}",
                self.clip(&outcome.final_text)
            )
        };
        let why = match &outcome.reason {
            FinishReason::MaxIterations { limit } => format!(
                "The sub-agent used up its {limit}-iteration budget before it finished. {said}\n\n\
                 Give it a narrower task (one question, the files to start from), or do the rest \
                 yourself."
            ),
            FinishReason::Stopped { stop_reason } => format!(
                "The sub-agent was stopped by the model provider ({stop_reason}) before it \
                 finished. {said}\n\nTry again with a narrower task, or do it yourself."
            ),
            // Its interrupt is never fired, and it does not finish on its own
            // in any other way; answered anyway so the call is.
            FinishReason::Interrupted | FinishReason::Completed => {
                format!("The sub-agent stopped before it finished. {said}\n\nDo the task yourself.")
            }
        };
        ToolError::execution(&Self::name(), why)
    }

    /// `answer`, cut to the limit with the cut stated.
    fn clip(&self, answer: &str) -> String {
        let (kept, truncated) = text::truncate_owned(answer, self.limits.max_answer_bytes);
        if truncated {
            format!(
                "{kept}\n\n[sub-agent answer truncated at {} bytes]",
                self.limits.max_answer_bytes
            )
        } else {
            kept
        }
    }
}

#[async_trait]
impl Tool for SpawnAgentTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: Self::name(),
            description: "Hand a self-contained investigation to a sub-agent and get back only \
                          its summary - not the files it read. Use it when answering a question \
                          would take many reads or searches whose output you do not need to \
                          keep (\"how is X wired?\", \"where is Y used, and how?\").\n\
                          The sub-agent starts with an empty conversation: it sees only `task`, \
                          so say everything it needs - the question, where to start, what the \
                          summary must contain. It can read and search the workspace, but cannot \
                          write, run commands, use the network or start sub-agents of its own. \
                          Independent investigations can be started together in one turn."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "task": {
                        "type": "string",
                        "description": "The complete instructions for the sub-agent, e.g. \
                                        `Find every caller of parse_config in crates/ and report \
                                        which ones ignore its error, with file:line.`"
                    }
                },
                "required": ["task"],
                "additionalProperties": false
            }),
            // The sub-agent can only read, and each of its calls goes through
            // the approval gate on its own; asking about the spawn as well
            // would be asking twice about nothing new.
            safety: ToolSafety::ReadOnly,
        }
    }

    async fn preview(&self, _arguments: &Value) -> Result<Vec<PlannedWrite>, ToolError> {
        // Writes nothing; its sub-agent cannot write either.
        Ok(Vec::new())
    }

    async fn execute(&self, arguments: Value) -> Result<ToolOutcome, ToolError> {
        let name = Self::name();
        let input: Input = parse_arguments(&name, arguments)?;
        let task = input.task.trim();
        if task.is_empty() {
            return Err(ToolError::invalid_input(
                &name,
                "`task` is empty. Write out the question for the sub-agent, where to start, and \
                 what its summary must contain.",
            ));
        }
        // Outside a dispatched call there is no session to name the
        // sub-agent's after (see `call_scope`). Refused rather than guessed:
        // an invented id could land the record in another session's file.
        let Some(scope) = call_scope::current() else {
            return Err(ToolError::execution(
                &name,
                "the sub-agent cannot be started outside the agent loop. Do the task yourself.",
            ));
        };

        let child = self.child.nested_under(scope.call_id.clone());
        let mut session = Session::new(subagent_session_id(&scope.session_id, &scope.call_id));
        // A fresh interrupt, never fired: the parent stops the sub-agent by
        // dropping this call, which drops the run below with it.
        let outcome = child
            .run_until(&mut session, task, &Interrupt::new())
            .await
            .map_err(|error| {
                ToolError::execution(
                    &name,
                    format!(
                        "the sub-agent could not reach the model ({error}), and nothing it found \
                         is available. Do the task yourself, or try again later."
                    ),
                )
            })?;
        if !outcome.is_complete() {
            return Err(self.unfinished(&outcome));
        }

        let answer = if outcome.final_text.trim().is_empty() {
            "The sub-agent finished without saying anything.".to_string()
        } else {
            self.clip(&outcome.final_text)
        };
        Ok(ToolOutcome::new(answer)
            .with_summary(summary(&outcome))
            .with_usage(outcome.usage))
    }
}

fn summary(outcome: &AgentOutcome) -> String {
    format!(
        "sub-agent finished in {} iteration(s), {} tokens",
        outcome.iterations,
        outcome.usage.total()
    )
}
