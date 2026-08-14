//! The agent loop.
//!
//! ```text
//!   user input
//!       |
//!       v
//!   +-------------------------------------------------------+
//!   | 1. trim history to the context budget                  |
//!   | 2. ask the model (system prompt + history + tool defs) |
//!   | 3. no tool calls?  -> answer the user, done            |
//!   | 4. dispatch the calls (see `dispatch.rs`)              |
//!   | 5. append the results, go to 1                         |
//!   +-------------------------------------------------------+
//!            bounded by `max_iterations`
//! ```
//!
//! The loop only aborts when the *provider* fails. A tool that fails, is
//! unknown, or is refused by the user comes back as an error-flagged
//! `tool_result`, because a model correcting its own mistake is the normal
//! case rather than an exception.

use std::sync::Arc;
use std::time::Instant;

use agent_domain::model::llm::{
    ChatRequest, ChatResponse, RequestMetadata, StopReason, TaskKind, TokenUsage,
};
use agent_domain::model::message::Message;
use agent_domain::model::tool::{ToolCall, ToolDefinition};
use agent_domain::ports::approval::ApprovalGate;
use agent_domain::ports::context::ContextProvider;
use agent_domain::ports::events::{AgentEvent, EventSink, FinishReason};
use agent_domain::ports::llm::{LlmProvider, StreamEvent};
use agent_domain::ports::prompt::PromptBuilder;
use futures::StreamExt as _;
use tracing::{debug, warn};

use super::config::AgentLoopConfig;
use super::dispatch::{DispatchConfig, ToolDispatcher};
use super::session::Session;
use crate::error::AppError;
use crate::tools::registry::ToolRegistry;

/// Result of one `run`.
#[derive(Debug, Clone, PartialEq)]
pub struct AgentOutcome {
    pub final_text: String,
    pub iterations: u32,
    pub usage: TokenUsage,
    pub reason: FinishReason,
}

impl AgentOutcome {
    /// True when the model produced a final answer rather than running out of
    /// iterations or being cut short by the provider.
    pub fn is_complete(&self) -> bool {
        matches!(self.reason, FinishReason::Completed)
    }
}

/// Everything the loop collaborates with, named so call sites read as wiring
/// rather than as a positional argument list.
///
/// Every field is a port: swap an implementation here and the loop is none the
/// wiser. New collaborators are added as fields, which keeps the churn at the
/// construction sites instead of in every signature between them.
pub struct AgentDependencies {
    pub llm: Arc<dyn LlmProvider>,
    pub tools: Arc<ToolRegistry>,
    pub approval: Arc<dyn ApprovalGate>,
    pub events: Arc<dyn EventSink>,
    pub context: Arc<dyn ContextProvider>,
    pub prompt: Arc<dyn PromptBuilder>,
}

pub struct AgentLoop {
    llm: Arc<dyn LlmProvider>,
    dispatcher: ToolDispatcher,
    events: Arc<dyn EventSink>,
    context: Arc<dyn ContextProvider>,
    prompt: Arc<dyn PromptBuilder>,
    config: AgentLoopConfig,
}

impl AgentLoop {
    pub fn new(deps: AgentDependencies, config: AgentLoopConfig) -> Self {
        let AgentDependencies {
            llm,
            tools,
            approval,
            events,
            context,
            prompt,
        } = deps;
        let dispatcher = ToolDispatcher::new(
            tools,
            approval,
            events.clone(),
            DispatchConfig {
                max_tool_output_bytes: config.max_tool_output_bytes,
                parallel_read_only_tools: config.parallel_read_only_tools,
            },
        );
        Self {
            llm,
            dispatcher,
            events,
            context,
            prompt,
            config,
        }
    }

    /// Runs one user turn to completion, mutating `session` in place so a REPL
    /// can call this repeatedly and keep the history.
    pub async fn run(
        &self,
        session: &mut Session,
        user_input: impl Into<String>,
    ) -> Result<AgentOutcome, AppError> {
        let tools = self.advertised_tools();
        let snapshot = self.context.snapshot().await?;
        let system_prompt = self.prompt.build(&snapshot, &tools);
        // A zero budget would silently answer nothing; one round-trip is the
        // smallest thing that can be called a run.
        let limit = self.config.max_iterations.max(1);

        session.conversation.push(Message::user(user_input));
        self.events.emit(AgentEvent::RunStarted {
            session_id: session.id.clone(),
            provider: self.llm.id(),
            model: self.config.model.clone(),
        });

        for iteration in 1..=limit {
            self.events
                .emit(AgentEvent::IterationStarted { iteration, limit });
            self.trim_history(session);

            let request = self.build_request(session, &system_prompt, &tools, iteration);
            let started = Instant::now();
            let response = if self.config.stream {
                self.chat_streaming(request).await?
            } else {
                self.llm.chat(request).await?
            };

            session.usage.accumulate(response.usage);
            self.events.emit(AgentEvent::ModelResponded {
                provider: response.provider.clone(),
                model: response.model.clone(),
                stop_reason: response.stop_reason.clone(),
                usage: response.usage,
                latency: started.elapsed(),
            });

            let text = response.message.text();
            if !text.trim().is_empty() {
                self.events
                    .emit(AgentEvent::AssistantMessage { text: text.clone() });
            }

            let calls: Vec<ToolCall> = response.message.tool_calls().cloned().collect();
            let stop_reason = response.stop_reason.clone();
            session.conversation.push(response.message);

            if calls.is_empty() {
                return Ok(self.finish(session, text, iteration, finish_reason_for(&stop_reason)));
            }

            debug!(iteration, calls = calls.len(), "dispatching tool calls");
            let results = self.dispatcher.dispatch(&calls).await;
            session.conversation.push(Message::tool_results(results));
        }

        // The model kept asking for tools until the budget ran out. Report what
        // we have rather than pretending the answer is final.
        let partial = session
            .conversation
            .last_assistant_text()
            .unwrap_or_else(|| {
                format!("Stopped after {limit} iterations without reaching a final answer.")
            });
        Ok(self.finish(
            session,
            partial,
            limit,
            FinishReason::MaxIterations { limit },
        ))
    }

    /// One model round-trip over the streaming interface.
    ///
    /// Prose fragments surface immediately as [`AgentEvent::AssistantDelta`];
    /// the turn is still driven solely by the final completed response, which
    /// carries any tool calls with fully-aggregated arguments. Providers that
    /// cannot stream yield only the completed response (no deltas), which is
    /// exactly the non-streamed rendering path - so this method behaves
    /// identically to [`LlmProvider::chat`] for them.
    async fn chat_streaming(&self, request: ChatRequest) -> Result<ChatResponse, AppError> {
        let mut stream = self.llm.chat_stream(request).await?;

        // Leading whitespace is held back: the non-streaming path never prints
        // a whitespace-only answer (some models emit "\n\n" before a tool
        // call), and the streamed output must be byte-identical to it.
        let mut held_whitespace = String::new();
        let mut live = false;

        while let Some(event) = stream.next().await {
            match event.map_err(AppError::from)? {
                StreamEvent::TextDelta(delta) => {
                    if live {
                        self.events.emit(AgentEvent::AssistantDelta { text: delta });
                    } else {
                        held_whitespace.push_str(&delta);
                        if !held_whitespace.trim().is_empty() {
                            live = true;
                            self.events.emit(AgentEvent::AssistantDelta {
                                text: std::mem::take(&mut held_whitespace),
                            });
                        }
                    }
                }
                // The protocol makes `Completed` the final event.
                StreamEvent::Completed(response) => return Ok(response),
            }
        }

        Err(AppError::Llm(
            agent_domain::error::LlmError::InvalidResponse(
                "the stream ended without a completed response".to_string(),
            ),
        ))
    }

    /// Tool definitions to advertise, or none if the provider cannot use them.
    fn advertised_tools(&self) -> Vec<ToolDefinition> {
        if self.llm.capabilities().supports_tools {
            self.dispatcher.definitions()
        } else {
            warn!(provider = %self.llm.id(), "provider reports no tool support; running tool-less");
            Vec::new()
        }
    }

    fn trim_history(&self, session: &mut Session) {
        let dropped = session.conversation.trim_to_budget(
            self.config.max_history_bytes,
            self.config.keep_recent_messages,
        );
        if dropped > 0 {
            self.events.emit(AgentEvent::HistoryTrimmed {
                dropped_messages: dropped,
            });
        }
    }

    fn build_request(
        &self,
        session: &Session,
        system_prompt: &str,
        tools: &[ToolDefinition],
        iteration: u32,
    ) -> ChatRequest {
        ChatRequest::new(session.conversation.messages().to_vec())
            .with_system(system_prompt)
            .with_tools(tools.to_vec())
            .with_model(self.config.model.clone())
            .with_params(self.config.params.clone())
            .with_metadata(RequestMetadata {
                session_id: session.id.clone(),
                iteration,
                task_kind: TaskKind::Agentic,
                requires_tools: !tools.is_empty(),
                hints: Default::default(),
            })
    }

    fn finish(
        &self,
        session: &Session,
        final_text: String,
        iterations: u32,
        reason: FinishReason,
    ) -> AgentOutcome {
        self.events.emit(AgentEvent::RunFinished {
            reason: reason.clone(),
            iterations,
            usage: session.usage,
        });
        AgentOutcome {
            final_text,
            iterations,
            usage: session.usage,
            reason,
        }
    }
}

fn finish_reason_for(stop_reason: &StopReason) -> FinishReason {
    match stop_reason {
        // `ToolUse` without any tool call means the provider labelled the turn
        // optimistically; the model still produced its answer.
        StopReason::EndTurn | StopReason::ToolUse => FinishReason::Completed,
        StopReason::MaxTokens => FinishReason::Stopped {
            stop_reason: "response hit the max_tokens limit".into(),
        },
        StopReason::StopSequence => FinishReason::Stopped {
            stop_reason: "a stop sequence was produced".into(),
        },
        StopReason::Other(other) => FinishReason::Stopped {
            stop_reason: other.clone(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_provider_stop_reasons() {
        assert_eq!(
            finish_reason_for(&StopReason::EndTurn),
            FinishReason::Completed
        );
        assert_eq!(
            finish_reason_for(&StopReason::ToolUse),
            FinishReason::Completed
        );
        assert!(matches!(
            finish_reason_for(&StopReason::MaxTokens),
            FinishReason::Stopped { .. }
        ));
        assert_eq!(
            finish_reason_for(&StopReason::Other("content_filter".into())),
            FinishReason::Stopped {
                stop_reason: "content_filter".into()
            }
        );
    }
}
