//! The agent loop.
//!
//! ```text
//!   user input
//!       |
//!       v
//!   +-------------------------------------------------------+
//!   | 1. fit the history to the context budget:              |
//!   |      compact (fold old turns into a summary), then     |
//!   |      trim (drop what still does not fit)               |
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
//!
//! An interrupt (see [`super::interrupt`]) is not a failure either: the turn
//! returns `Ok` with [`FinishReason::Interrupted`], after making sure the
//! history it leaves behind is one the next request can be built on. It is
//! looked for at the top of each iteration, during a compaction, during the
//! model request, and inside the dispatcher - nowhere else, because between
//! those points the loop is only appending what it already has.

use std::sync::Arc;
use std::time::Instant;

use agent_domain::model::llm::{
    ChatRequest, ChatResponse, ModelId, RequestMetadata, StopReason, TaskKind, TokenUsage,
};
use agent_domain::model::message::Message;
use agent_domain::model::tool::{ToolCall, ToolCallId, ToolDefinition};
use agent_domain::ports::approval::ApprovalGate;
use agent_domain::ports::context::ContextProvider;
use agent_domain::ports::conversation_log::ConversationLog;
use agent_domain::ports::events::{AgentEvent, EventSink, FinishReason};
use agent_domain::ports::llm::{LlmProvider, StreamEvent};
use agent_domain::ports::prompt::PromptBuilder;
use futures::StreamExt as _;
use tracing::{debug, warn};

use super::compaction::{CompactionConfig, CompactionReport, HistoryCompactor, ManualCompaction};
use super::config::AgentLoopConfig;
use super::context_report::{ContextReport, PlanNote, definition_bytes};
use super::dispatch::{DispatchConfig, ToolDispatcher};
use super::interrupt::Interrupt;
use super::plan;
use super::session::Session;
use crate::checkpoint::{self, CheckpointError, CheckpointJournal, RestoreReport, RewindReport};
use crate::error::AppError;
use crate::tools::registry::ToolRegistry;

/// How far down a trim goes once it has to trim at all: three quarters of
/// the budget.
///
/// Trimming only to the budget itself would, once a conversation reaches it,
/// drop a message or two from the front on every iteration - and every
/// change at the front rewrites the whole message prefix the provider has
/// cached, turning a cache read (0.1x) into a cache write (1.25x) on each
/// request. Dropping a quarter at once buys several iterations with an
/// unchanged prefix.
fn trim_low_water(budget: usize) -> usize {
    budget - budget / 4
}

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
    /// Where the conversation is written down before the loop is allowed to
    /// shorten it. [`NullConversationLog`](agent_domain::ports::conversation_log::NullConversationLog)
    /// writes nothing, which is both the off switch and what the tests use.
    pub log: Arc<dyn ConversationLog>,
}

pub struct AgentLoop {
    llm: Arc<dyn LlmProvider>,
    dispatcher: ToolDispatcher,
    compactor: HistoryCompactor,
    events: Arc<dyn EventSink>,
    context: Arc<dyn ContextProvider>,
    prompt: Arc<dyn PromptBuilder>,
    log: Arc<dyn ConversationLog>,
    /// Where the writes of each turn are recorded for `/undo` and `/rewind`.
    /// `None` leaves both unavailable; see [`Self::with_checkpoints`].
    checkpoints: Option<Arc<CheckpointJournal>>,
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
            log,
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
        // Built here rather than taken as a dependency: it needs the same
        // provider the loop already holds, and giving callers a second one to
        // wire would let the two drift apart.
        let compactor = HistoryCompactor::new(
            llm.clone(),
            CompactionConfig {
                enabled: config.compact,
                keep_recent_messages: config.compact_keep_recent,
                keep_recent_bytes: config.compact_keep_recent_bytes,
                model: config.model.clone(),
            },
        );
        Self {
            llm,
            dispatcher,
            compactor,
            events,
            context,
            prompt,
            log,
            checkpoints: None,
            config,
        }
    }

    /// Opens a turn in `journal` each time a user message is pushed, so the
    /// writes [`CheckpointingTool`](crate::checkpoint::CheckpointingTool)
    /// records there are grouped by the turn that made them.
    ///
    /// A builder rather than a field of [`AgentDependencies`]: it is not a
    /// port, and a loop without it is complete - it just cannot undo.
    #[must_use]
    pub fn with_checkpoints(mut self, journal: Arc<CheckpointJournal>) -> Self {
        self.checkpoints = Some(journal);
        self
    }

    /// This loop, narrating to `events` instead - every event it emits,
    /// its dispatcher's and its compactions' included. Everything else is
    /// shared, not copied.
    fn with_events(&self, events: Arc<dyn EventSink>) -> Self {
        Self {
            llm: self.llm.clone(),
            dispatcher: self.dispatcher.with_events(events.clone()),
            compactor: self.compactor.clone(),
            events,
            context: self.context.clone(),
            prompt: self.prompt.clone(),
            log: self.log.clone(),
            checkpoints: self.checkpoints.clone(),
            config: self.config.clone(),
        }
    }

    /// This loop as the sub-agent started by the call `parent_call_id`: each
    /// of its events reaches this loop's sink wrapped in
    /// [`AgentEvent::Nested`], so whoever reads them can tell the child's
    /// work - and above all its answer - from the parent's.
    pub(crate) fn nested_under(&self, parent_call_id: ToolCallId) -> Self {
        self.with_events(Arc::new(NestedSink {
            parent: self.events.clone(),
            parent_call_id,
        }))
    }

    /// Takes back the files the newest turn that wrote any changed, leaving
    /// the conversation as it is; the model is told on its next turn.
    pub async fn undo(&self, session: &mut Session) -> Result<RestoreReport, CheckpointError> {
        let journal = self.checkpoints.as_ref().ok_or(CheckpointError::Disabled)?;
        checkpoint::undo(journal, session).await
    }

    /// Takes the files and the conversation back to before the `turns`-th
    /// newest turn, and marks the log so a resume does the same.
    pub async fn rewind(
        &self,
        session: &mut Session,
        turns: usize,
    ) -> Result<RewindReport, CheckpointError> {
        let journal = self.checkpoints.as_ref().ok_or(CheckpointError::Disabled)?;
        checkpoint::rewind(journal, self.log.as_ref(), session, turns).await
    }

    /// Runs one user turn to completion, mutating `session` in place so a REPL
    /// can call this repeatedly and keep the history.
    pub async fn run(
        &self,
        session: &mut Session,
        user_input: impl Into<String>,
    ) -> Result<AgentOutcome, AppError> {
        self.run_until(session, user_input, &Interrupt::new()).await
    }

    /// [`Self::run`], stopping early once `interrupt` fires.
    ///
    /// An interrupted turn returns `Ok` with [`FinishReason::Interrupted`] and
    /// leaves `session` ready for the next one: every `tool_call` in it has a
    /// result, in memory and in the log alike. Nothing is added to say that it
    /// was interrupted beyond those results, and nothing is taken away - a
    /// half-streamed answer was never part of the history, and a call the
    /// model made stays where it was, answered.
    pub async fn run_until(
        &self,
        session: &mut Session,
        user_input: impl Into<String>,
        interrupt: &Interrupt,
    ) -> Result<AgentOutcome, AppError> {
        let tools = self.advertised_tools();
        let system_prompt = self.system_prompt(session, &tools).await?;
        // A zero budget would silently answer nothing; one round-trip is the
        // smallest thing that can be called a run.
        let limit = self.config.max_iterations.max(1);

        let user_input = session.with_notes(user_input.into());
        session.conversation.push(Message::user(user_input));
        self.record(session, 1).await;
        if let (Some(journal), Some(seq)) = (
            &self.checkpoints,
            session.conversation.messages().last().and_then(|m| m.seq),
        ) {
            journal.begin_turn(seq);
        }
        self.events.emit(AgentEvent::RunStarted {
            session_id: session.id.clone(),
            provider: self.llm.id(),
            model: self.model(session),
        });

        for iteration in 1..=limit {
            if interrupt.is_fired() {
                return Ok(self.interrupted(session, iteration - 1));
            }
            self.events
                .emit(AgentEvent::IterationStarted { iteration, limit });
            if self.fit_history(session, interrupt).await.is_none() {
                return Ok(self.interrupted(session, iteration));
            }

            let request = self.build_request(session, &system_prompt, &tools, iteration);
            let started = Instant::now();
            // Abandoning the request leaves nothing to undo: the response is
            // only pushed once it is complete, so whatever was streamed so far
            // was shown and is gone.
            let pending = async {
                if self.config.stream {
                    self.chat_streaming(request).await
                } else {
                    self.llm.chat(request).await.map_err(AppError::from)
                }
            };
            let Some(response) = interrupt.guard(pending).await else {
                return Ok(self.interrupted(session, iteration));
            };
            let response = response?;

            session.usage.accumulate(response.usage);
            session.last_usage = Some(response.usage);
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
            self.record(session, 1).await;

            if calls.is_empty() {
                return Ok(self.finish(session, text, iteration, finish_reason_for(&stop_reason)));
            }

            debug!(iteration, calls = calls.len(), "dispatching tool calls");
            // Answers every call even when interrupted, so the results are
            // pushed and recorded before the interrupt is acted on - never
            // instead of it.
            let dispatched = self
                .dispatcher
                .dispatch_in(&session.id, &calls, interrupt)
                .await;
            // What a sub-agent spent is this turn's cost too. Not in
            // `last_usage`: that one measures this context, not the child's.
            session.usage.accumulate(dispatched.usage);
            session
                .conversation
                .push(Message::tool_results(dispatched.results));
            self.record(session, 1).await;
            // The call and its result, read now: by the next shortening the
            // call that wrote the plan may already be gone.
            if let Some(plan) = plan::refresh(session, 2) {
                self.events.emit(AgentEvent::PlanUpdated { plan });
            }
            if interrupt.is_fired() {
                return Ok(self.interrupted(session, iteration));
            }
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

    /// The session's system prompt, rendered on its first run and reused by
    /// every later one - see [`Session`] for why it is not rebuilt.
    async fn system_prompt(
        &self,
        session: &mut Session,
        tools: &[ToolDefinition],
    ) -> Result<String, AppError> {
        if let Some(prompt) = &session.system_prompt {
            return Ok(prompt.clone());
        }
        let snapshot = self.context.snapshot().await?;
        let prompt = self.prompt.build(&snapshot, tools);
        session.system_prompt = Some(prompt.clone());
        Ok(prompt)
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

    /// Writes the `appended` newest messages to the durable record.
    ///
    /// Called immediately after every push, and the timing is the whole point.
    /// [`Self::fit_history`] runs at the top of the *next* iteration, and both
    /// of the things it does are irreversible: compaction replaces old turns
    /// with a summary, trimming deletes them outright. Whatever has not been
    /// written by the time it runs is what the record will be missing, so this
    /// happens while the message is still whole.
    ///
    /// The messages are taken back out of the conversation rather than logged
    /// on the way in, because that is where they have their sequence numbers.
    ///
    /// A failure is warned about and dropped. Logging is a service to whoever
    /// reads the session afterwards; a turn that dies because it could not be
    /// written down helps nobody, least of all them.
    async fn record(&self, session: &Session, appended: usize) {
        let messages = session.conversation.messages();
        let from = messages.len().saturating_sub(appended);

        if let Err(error) = self.log.append(&session.id, &messages[from..]).await {
            warn!(%error, session = %session.id, "cannot write the conversation log");
        }
    }

    /// Brings the history back inside its budget before the next request.
    ///
    /// Two stages, and the order is the point. Compaction is the one that
    /// preserves meaning, so it goes first and gets the whole history to work
    /// with. Trimming runs afterwards regardless, because it is the only stage
    /// that *guarantees* the budget: a summary that failed to arrive, or one
    /// that landed on top of a tail already filling the budget, still has to be
    /// dealt with. In that second case trimming will drop the summary itself -
    /// the budget is a hard limit and something has to go.
    ///
    /// When either stage shortened the history and the model has unfinished
    /// work it can no longer see, its plan is appended once as a note (see
    /// [`super::plan`]). Room for that note is reserved in the budget up
    /// front, whenever there is unfinished work and no copy of the note is in
    /// the history already: without the reservation the note would push a
    /// history trimmed to exactly the budget back over it, and the next
    /// iteration would trim again just to make room for it. A copy already in
    /// the history is counted where it is, not a second time.
    ///
    /// A trim goes down to [`trim_low_water`], not to the budget, unless a
    /// compaction just ran.
    ///
    /// `None` when `interrupt` fired during the compaction. The summary is
    /// abandoned before it changed anything, and trimming is skipped too: the
    /// turn is ending, and the next one fits the history again first.
    async fn fit_history(&self, session: &mut Session, interrupt: &Interrupt) -> Option<()> {
        self.shorten(session, interrupt, false).await.map(|_| ())
    }

    /// [`Self::fit_history`], with the compaction it may run returned - and,
    /// when `forced`, run whether or not the history is over its budget or
    /// compaction is switched on. `/compact` and the loop share this so a
    /// compaction asked for by hand restores the plan exactly as one the loop
    /// decided on does.
    async fn shorten(
        &self,
        session: &mut Session,
        interrupt: &Interrupt,
        forced: bool,
    ) -> Option<Option<CompactionReport>> {
        let reservation = plan::reserve(self.config.max_history_bytes, session);
        let budget = reservation.budget;

        let wanted =
            forced || (self.compactor.enabled() && session.conversation.approx_bytes() > budget);
        let mut compaction = None;
        if wanted {
            let model = self.model(session);
            compaction = interrupt
                .guard(
                    self.compactor
                        .fold(&session.id, &mut session.conversation, model),
                )
                .await?;
        }
        if let Some(report) = &compaction {
            session.compactions += 1;
            session.usage.accumulate(report.usage);
            self.events.emit(AgentEvent::HistoryCompacted {
                folded_messages: report.folded_messages,
                before_bytes: report.before_bytes,
                after_bytes: report.after_bytes,
            });
        }
        let compacted = compaction.is_some();

        // A summary just made room on its own terms; trimming below the
        // budget on top of it would delete what was just kept verbatim.
        let low_water = if compacted {
            budget
        } else {
            trim_low_water(budget)
        };
        let keep_recent = self.config.keep_recent_messages;
        let mut dropped = session
            .conversation
            .trim_to_budget(budget, low_water, keep_recent);

        if (compacted || dropped > 0)
            && let Some(restore) = reservation.restore(session)
        {
            if let Some(budget) = restore.make_room {
                dropped += session.conversation.trim_to_budget(
                    budget,
                    trim_low_water(budget),
                    keep_recent,
                );
            }
            session.conversation.push(restore.note);
            self.record(session, 1).await;
        }
        if dropped > 0 {
            self.events.emit(AgentEvent::HistoryTrimmed {
                dropped_messages: dropped,
            });
        }
        Some(compaction)
    }

    /// Folds the session's older history into a summary now, between turns,
    /// whatever the budget says and even with automatic compaction switched
    /// off: whoever asks has decided the request is worth it. The summary
    /// request's usage is added to the session's.
    ///
    /// The same procedure the loop runs before a request, plan note included
    /// - see [`Self::fit_history`].
    pub async fn compact_now(
        &self,
        session: &mut Session,
        interrupt: &Interrupt,
    ) -> ManualCompaction {
        if !self.compactor.has_foldable(&session.conversation) {
            return ManualCompaction::NothingToFold;
        }
        match self.shorten(session, interrupt, true).await {
            Some(Some(report)) => ManualCompaction::Compacted(report),
            Some(None) => ManualCompaction::Failed,
            None => ManualCompaction::Interrupted,
        }
    }

    /// The model `session`'s requests go to: the one chosen for it with
    /// [`Session::switch_model`], else the configured one. `None` leaves the
    /// choice to the provider.
    pub fn model(&self, session: &Session) -> Option<ModelId> {
        session
            .model_override
            .clone()
            .or_else(|| self.config.model.clone())
    }

    /// Where the bytes of `session`'s next request would go, for `/context`.
    ///
    /// Reads only: the system prompt is reported as it was rendered, or as
    /// not rendered yet - never rendered here, since rendering it fixes it
    /// for the rest of the session.
    pub fn context_report(&self, session: &Session) -> ContextReport {
        let tools = self.advertised_tools();
        let plan_note = plan::reminder(session.plan()).map(|note| PlanNote {
            bytes: note.approx_bytes(),
            in_history: plan::already_noted(session, &note),
        });
        ContextReport {
            model: self.model(session),
            system_bytes: session.system_prompt.as_ref().map(String::len),
            tool_count: tools.len(),
            tools_bytes: tools.iter().map(definition_bytes).sum(),
            history_bytes: session.conversation.approx_bytes(),
            messages: session.conversation.len(),
            history_budget: self.config.max_history_bytes,
            plan_note,
            last_usage: session.last_usage,
            compactions: session.compactions,
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
            .with_model(self.model(session))
            .with_params(self.config.params.clone())
            .with_metadata(RequestMetadata {
                session_id: session.id.clone(),
                iteration,
                task_kind: TaskKind::Agentic,
                requires_tools: !tools.is_empty(),
                hints: Default::default(),
            })
    }

    /// The outcome of a turn that stopped because it was asked to. No final
    /// text: there is no answer, and inventing one would put words in the
    /// model's mouth.
    fn interrupted(&self, session: &Session, iterations: u32) -> AgentOutcome {
        self.finish(
            session,
            String::new(),
            iterations,
            FinishReason::Interrupted,
        )
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

/// Hands a sub-agent's events to its parent's sink, each marked with the call
/// that started it.
struct NestedSink {
    parent: Arc<dyn EventSink>,
    parent_call_id: ToolCallId,
}

impl EventSink for NestedSink {
    fn emit(&self, event: AgentEvent) {
        self.parent.emit(AgentEvent::Nested {
            parent_call_id: self.parent_call_id.clone(),
            event: Box::new(event),
        });
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
