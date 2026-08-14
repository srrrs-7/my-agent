//! History compaction: folding old turns into a summary instead of dropping
//! them.
//!
//! ## Why this exists
//!
//! Trimming keeps the history inside its budget by deleting the oldest
//! messages. What it deletes first is the *beginning* of the session - which is
//! where the user said what they wanted. An agent that has forgotten the
//! request while still holding the last three tool results is worse than one
//! that remembers the request and little else.
//!
//! ## What the summary is for
//!
//! Not a readable account of the conversation: a handover to the agent that
//! continues the work. The prompt below asks for decisions and their reasons,
//! for what was already tried and failed, and - above all - for every user
//! message. Those are the things whose loss shows up as the model re-asking a
//! settled question or re-running a fix that did not work.
//!
//! ## Recency is a weight, not a cliff
//!
//! Two things decide what survives, and both are measured in messages and
//! bytes rather than in seconds.
//!
//! The tail kept word for word is bounded by size as well as by count, so how
//! many recent messages survive depends on how big they turned out to be - a
//! turn that read a 32 KB file does not get to protect itself twelve times
//! over. Inside the folded prefix the thinning is gradual: a block's share of
//! the transcript halves the further back it sits, down to a floor it never
//! crosses, so the turns just behind the tail reach the summariser in detail
//! and the ones at the bottom reach it as an outline.
//!
//! There is no wall clock anywhere in this. What matters is how much
//! conversation has happened since something was said, not how much time - an
//! agent whose session sat idle for three hours has not moved on from
//! anything, and a clock would tell it that it had.
//!
//! ## Failure is not an error
//!
//! Compaction is an improvement on trimming, not a precondition for it. If the
//! model refuses, times out, or answers with nothing, this returns `None` and
//! the loop trims as it always did. Nothing is mutated until a usable summary
//! is in hand, so there is no half-compacted state to recover from.

use std::sync::Arc;

use agent_domain::model::conversation::Conversation;
use agent_domain::model::llm::{ChatRequest, GenerationParams, ModelId, RequestMetadata, TaskKind};
use agent_domain::model::message::{ContentBlock, Message, Role};
use agent_domain::ports::llm::LlmProvider;
use agent_domain::text;
use tracing::{debug, warn};

/// Room for the summary itself. Large enough for the structure the prompt asks
/// for, small enough that the result cannot itself dominate the budget it was
/// meant to relieve.
const SUMMARY_MAX_TOKENS: u32 = 2048;

/// Per-block cap for the most recent of the folded messages - with one
/// exception, [`render_transcript`].
///
/// The prefix is being folded precisely because it is too big to keep, so
/// sending all of it back would defeat the exercise. Tool output is most of the
/// bulk and the least worth preserving word for word: what the summary needs is
/// that a file was read and what came of it, not the file.
const MAX_BLOCK_BYTES: usize = 2_000;

/// Per-block cap for anything far enough back, and a floor that is never
/// crossed.
///
/// A block reduced to nothing is a block the summary cannot mention at all,
/// and "there was a step here I can no longer describe" is worse than a short
/// description of it.
const MIN_BLOCK_BYTES: usize = 250;

/// How many messages of distance halve a block's share of the transcript.
///
/// Eight is roughly one exchange plus the tool calls it took: near enough that
/// the thinning is felt within the span the next request is likely to reach
/// back into, far enough that it is not felt inside a single turn.
const HALVING_DISTANCE: u64 = 8;

/// How much of one block survives into the transcript, given how many messages
/// back it sits.
///
/// The retention rule of this whole module in one line: recency is a weight,
/// not a cliff. Everything in the prefix is being replaced by a summary, but
/// the turns just behind the surviving tail are the ones the next request is
/// most likely to reach for, so they reach the summariser in detail while the
/// ones far behind reach it as an outline.
///
/// Distance is counted in *messages*, not in seconds. The question worth
/// asking is how much conversation has happened since, and an agent that spent
/// three hours idle has not moved on from anything.
fn block_budget(distance: u64) -> usize {
    let halvings = u32::try_from(distance / HALVING_DISTANCE).unwrap_or(u32::MAX);
    MAX_BLOCK_BYTES
        .checked_shr(halvings)
        .unwrap_or(0)
        .max(MIN_BLOCK_BYTES)
}

/// What the summariser is asked to produce.
///
/// The shape is deliberate. A free-form "summarise this" yields readable prose
/// that has quietly dropped the constraints the user attached to their request,
/// and a model has no way to know that its own reasoning is the cheapest thing
/// in the transcript to lose while a one-line user correction is the most
/// expensive. Naming the sections is what makes the omission visible.
const SUMMARY_INSTRUCTIONS: &str = "\
You are compacting the transcript of a coding-agent session so the work can continue in a \
fresh context window.

What you write replaces the messages you were given. Anything you leave out is gone - the \
agent cannot go back for it. Write for the agent picking the work up, not for a human \
reader: specifics beat brevity, and a detail you are unsure of is worth more recorded with \
its uncertainty than dropped.

Produce these sections, in order, omitting none. Write \"none\" where a section is genuinely \
empty.

1. Request and intent - what the user asked for, in their terms, with every constraint and \
every correction they made along the way.
2. Every user message - each one, in order, verbatim or close to it. Do not merge them and \
do not paraphrase an instruction away. This is the most expensive thing to lose.
3. Decisions and their reasons - what was chosen, what was rejected, and why. A decision \
recorded without its reason gets argued again from scratch.
4. Files and code - every path read or changed, what changed in each, and the exact \
identifiers, signatures and snippets that later work depends on.
5. What failed - errors hit, fixes applied, approaches ruled out and why. Without this the \
agent repeats them.
6. Current state - what is finished, what is verified, and what is neither.
7. Next step - the task that was in progress, if any, in the words it was last described in.

Rules:

- If the transcript already opens with a summary, you are extending a record rather than \
replacing one: carry forward everything in it that is still true.
- Preserve exact paths, identifiers, commands, flags and error text. Do not tidy names or \
round numbers.
- Do not settle an open question by guessing. Record it as open.
- Output the summary only. No preamble, no closing offer of help.";

#[derive(Debug, Clone, PartialEq)]
pub struct CompactionConfig {
    pub enabled: bool,
    /// Most messages the tail may keep verbatim. Everything before them is
    /// folded.
    pub keep_recent_messages: usize,
    /// Most bytes the tail may keep verbatim, whatever that works out to in
    /// messages.
    ///
    /// The cap that actually binds. A count alone protects twelve messages
    /// whether they are twelve one-line answers or twelve 32 KB tool results,
    /// and in the second case the tail that was meant to be protected is what
    /// pushes the fresh summary back out of the budget again.
    pub keep_recent_bytes: usize,
    /// Passed through so an operator who pinned a model gets it here too. A
    /// router that wants to send summaries somewhere cheaper reads
    /// [`TaskKind::Summarize`] off the request metadata instead.
    pub model: Option<ModelId>,
}

impl Default for CompactionConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            keep_recent_messages: 12,
            keep_recent_bytes: 64 * 1024,
            model: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompactionReport {
    pub folded_messages: usize,
    pub before_bytes: usize,
    pub after_bytes: usize,
}

pub struct HistoryCompactor {
    llm: Arc<dyn LlmProvider>,
    config: CompactionConfig,
}

impl HistoryCompactor {
    pub fn new(llm: Arc<dyn LlmProvider>, config: CompactionConfig) -> Self {
        Self { llm, config }
    }

    /// Folds the old part of `conversation` into a summary.
    ///
    /// `None` means the history was left untouched - either there was nothing
    /// worth folding or the summary could not be produced. Both are ordinary
    /// outcomes, and both leave trimming to do its job.
    pub async fn compact(
        &self,
        session_id: &str,
        conversation: &mut Conversation,
    ) -> Option<CompactionReport> {
        if !self.config.enabled {
            return None;
        }

        let split = conversation.compaction_split_within(
            self.config.keep_recent_messages,
            self.config.keep_recent_bytes,
        )?;
        let before_bytes = conversation.approx_bytes();
        let transcript = render_transcript(conversation, split);

        let summary = match self.llm.chat(self.request(session_id, transcript)).await {
            Ok(response) => response.message.text(),
            Err(error) => {
                warn!(%error, "history compaction failed; falling back to trimming");
                return None;
            }
        };
        if summary.trim().is_empty() {
            warn!("the summariser returned nothing; falling back to trimming");
            return None;
        }

        let folded = conversation.replace_prefix(split, summary_message(&summary));
        let after_bytes = conversation.approx_bytes();
        debug!(folded, before_bytes, after_bytes, "compacted the history");

        Some(CompactionReport {
            folded_messages: folded,
            before_bytes,
            after_bytes,
        })
    }

    /// The transcript arrives as one user message rather than as the original
    /// messages.
    ///
    /// Replaying the real turns would mean sending assistant `tool_call` blocks
    /// on a request that declares no tools, which providers disagree about
    /// accepting - and this is the one request that must work on every backend,
    /// since it runs when the session is already in trouble. Flattening also
    /// makes the clipping above possible, which replaying does not.
    fn request(&self, session_id: &str, transcript: String) -> ChatRequest {
        ChatRequest::new(vec![Message::user(transcript)])
            .with_system(SUMMARY_INSTRUCTIONS)
            .with_model(self.config.model.clone())
            .with_params(GenerationParams {
                // A record, not a composition: the same transcript should not
                // summarise differently on a retry.
                temperature: Some(0.0),
                max_tokens: Some(SUMMARY_MAX_TOKENS),
                top_p: None,
                stop_sequences: Vec::new(),
            })
            .with_metadata(RequestMetadata {
                session_id: session_id.to_string(),
                // Not part of the agent loop's own iteration count; this is a
                // side call, and reporting an iteration would misattribute it.
                iteration: 0,
                task_kind: TaskKind::Summarize,
                requires_tools: false,
                hints: Default::default(),
            })
    }
}

/// How the summary re-enters the conversation.
///
/// A *user* message, for two reasons. After a compaction the summary is the
/// opening message, and providers expect a conversation to open with a user
/// turn. An assistant turn would also read as something the model itself said,
/// and a model treats its own words as settled reasoning rather than as a
/// handover it should check.
///
/// The framing says plainly what the text is, because the alternative failure
/// is a model that answers the summary instead of continuing the work.
fn summary_message(summary: &str) -> Message {
    Message::user(format!(
        "The earlier turns of this conversation were folded into the record below to stay \
         within the context budget. It is what has already happened - not a new request, and \
         not something to act on by itself. Continue the work from where it leaves off.\n\n\
         <conversation-summary>\n{}\n</conversation-summary>",
        summary.trim()
    ))
}

/// Flattens `conversation.messages()[..split]` into a transcript the
/// summariser can read, spending more of the transcript on the turns nearest
/// the surviving tail than on the ones at the bottom.
///
/// How much each block gets is [`block_budget`] of its distance from the newest
/// message, which is why this needs the whole conversation and not just the
/// slice: distance is measured against the session, and the slice does not know
/// what comes after it.
///
/// **User messages are never clipped, at any distance.** They are the
/// instructions the whole session is an answer to, and a summary that has lost
/// half of one is worse than no compaction at all. It is also what protects the
/// *previous* summary, which re-enters as a user message: without the
/// exemption, the record written by the last compaction would be the oldest
/// thing in the history and so the first thing this one thinned away - each
/// compaction quietly undoing the one before it.
fn render_transcript(conversation: &Conversation, split: usize) -> String {
    let mut out = String::new();

    for (index, message) in conversation.messages()[..split].iter().enumerate() {
        let budget = block_budget(conversation.distance_from_newest(index));

        for block in &message.content {
            match block {
                ContentBlock::Text { text } if message.role == Role::User => {
                    out.push_str("\n## user\n");
                    out.push_str(text);
                    out.push('\n');
                }
                ContentBlock::Text { text } => {
                    out.push_str("\n## assistant\n");
                    out.push_str(&text::clip(text, budget));
                    out.push('\n');
                }
                ContentBlock::ToolCall(call) => {
                    out.push_str(&format!(
                        "\n### tool call: {}\n{}\n",
                        call.name,
                        text::clip(&call.arguments.to_string(), budget)
                    ));
                }
                ContentBlock::ToolResult(result) => {
                    out.push_str(&format!(
                        "\n### tool result: {} [{}]\n{}\n",
                        result.tool_name,
                        if result.is_error { "error" } else { "ok" },
                        text::clip(&result.content, budget)
                    ));
                }
            }
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_domain::error::LlmError;
    use agent_domain::model::llm::{
        ChatResponse, ProviderCapabilities, ProviderId, StopReason, TokenUsage,
    };
    use agent_domain::model::tool::{ToolCall, ToolCallId, ToolName, ToolResult};
    use async_trait::async_trait;
    use serde_json::json;
    use std::sync::Mutex;

    struct StubProvider {
        answer: Result<String, LlmError>,
        seen: Mutex<Option<ChatRequest>>,
    }

    impl StubProvider {
        fn answering(text: &str) -> Arc<Self> {
            Arc::new(Self {
                answer: Ok(text.to_string()),
                seen: Mutex::new(None),
            })
        }

        fn failing() -> Arc<Self> {
            Arc::new(Self {
                answer: Err(LlmError::InvalidResponse("no".into())),
                seen: Mutex::new(None),
            })
        }

        fn request(&self) -> ChatRequest {
            self.seen
                .lock()
                .unwrap()
                .clone()
                .expect("no request was made")
        }
    }

    #[async_trait]
    impl LlmProvider for StubProvider {
        fn id(&self) -> ProviderId {
            ProviderId::new("stub")
        }

        fn capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities::default()
        }

        async fn chat(&self, request: ChatRequest) -> Result<ChatResponse, LlmError> {
            *self.seen.lock().unwrap() = Some(request);
            match &self.answer {
                Ok(text) => Ok(ChatResponse {
                    message: Message::assistant_text(text.clone()),
                    stop_reason: StopReason::EndTurn,
                    usage: TokenUsage::default(),
                    provider: ProviderId::new("stub"),
                    model: ModelId::new("stub-model"),
                }),
                Err(error) => Err(error.clone()),
            }
        }
    }

    fn call() -> ToolCall {
        ToolCall::new(
            ToolCallId::new("c1"),
            ToolName::new("read_file").unwrap(),
            json!({"path": "a.rs"}),
        )
    }

    /// Eight messages, one of them a full tool turn.
    fn history() -> Conversation {
        let call = call();
        Conversation::from_messages(vec![
            Message::user("build a parser, and keep it dependency-free"),
            Message::assistant_text("looking"),
            Message::assistant(vec![ContentBlock::ToolCall(call.clone())]),
            Message::tool_results(vec![ToolResult::ok(&call, "x".repeat(50_000))]),
            Message::assistant_text("found it"),
            Message::user("also handle CRLF"),
            Message::assistant_text("done"),
            Message::user("now what"),
        ])
    }

    /// A byte cap nothing in these fixtures can exhaust, so the tests that
    /// were written against the message count still measure the message count.
    fn compactor(llm: Arc<StubProvider>, keep_recent: usize) -> HistoryCompactor {
        compactor_within(llm, keep_recent, 1024 * 1024)
    }

    fn compactor_within(
        llm: Arc<StubProvider>,
        keep_recent: usize,
        keep_recent_bytes: usize,
    ) -> HistoryCompactor {
        HistoryCompactor::new(
            llm,
            CompactionConfig {
                enabled: true,
                keep_recent_messages: keep_recent,
                keep_recent_bytes,
                model: None,
            },
        )
    }

    #[tokio::test]
    async fn folds_the_old_turns_and_keeps_the_recent_ones() {
        let mut conversation = history();
        let report = compactor(StubProvider::answering("RECORD"), 2)
            .compact("s1", &mut conversation)
            .await
            .expect("a compaction should have happened");

        assert_eq!(report.folded_messages, 6);
        assert_eq!(conversation.len(), 3, "summary plus the two kept messages");
        assert!(conversation.messages()[0].text().contains("RECORD"));
        assert_eq!(conversation.messages()[2].text(), "now what");
        assert!(
            report.after_bytes < report.before_bytes,
            "compaction must shrink the history: {report:?}"
        );
    }

    #[tokio::test]
    async fn the_summary_is_framed_as_a_record_rather_than_a_request() {
        let mut conversation = history();
        compactor(StubProvider::answering("RECORD"), 2)
            .compact("s1", &mut conversation)
            .await
            .unwrap();

        let head = &conversation.messages()[0];
        assert_eq!(
            head.role,
            Role::User,
            "a compacted history must open with a user message"
        );
        assert!(head.text().contains("not a new request"), "{}", head.text());
        assert!(head.text().contains("<conversation-summary>"));
    }

    #[tokio::test]
    async fn a_provider_failure_leaves_the_history_untouched() {
        let mut conversation = history();
        let before = conversation.clone();

        assert!(
            compactor(StubProvider::failing(), 2)
                .compact("s1", &mut conversation)
                .await
                .is_none()
        );
        assert_eq!(
            conversation, before,
            "nothing may be lost when the model fails"
        );
    }

    #[tokio::test]
    async fn an_empty_summary_is_treated_as_a_failure() {
        // Worse than an error: it would replace the history with nothing while
        // looking like a success.
        let mut conversation = history();
        let before = conversation.clone();

        assert!(
            compactor(StubProvider::answering("   \n "), 2)
                .compact("s1", &mut conversation)
                .await
                .is_none()
        );
        assert_eq!(conversation, before);
    }

    #[tokio::test]
    async fn disabled_means_disabled() {
        let mut conversation = history();
        let before = conversation.clone();
        let compactor = HistoryCompactor::new(
            StubProvider::answering("RECORD"),
            CompactionConfig {
                enabled: false,
                ..CompactionConfig::default()
            },
        );

        assert!(compactor.compact("s1", &mut conversation).await.is_none());
        assert_eq!(conversation, before);
    }

    #[tokio::test]
    async fn a_short_history_is_not_worth_a_round_trip() {
        let mut conversation = Conversation::from_messages(vec![Message::user("hi")]);
        assert!(
            compactor(StubProvider::answering("RECORD"), 12)
                .compact("s1", &mut conversation)
                .await
                .is_none()
        );
    }

    #[tokio::test]
    async fn the_request_is_labelled_for_routing_and_carries_no_tools() {
        let llm = StubProvider::answering("RECORD");
        let mut conversation = history();
        compactor(llm.clone(), 2)
            .compact("session-7", &mut conversation)
            .await
            .unwrap();

        let request = llm.request();
        assert_eq!(request.metadata.task_kind, TaskKind::Summarize);
        assert!(!request.metadata.requires_tools);
        assert_eq!(request.metadata.session_id, "session-7");
        assert!(
            request.tools.is_empty(),
            "a summary request must not advertise tools"
        );
        assert_eq!(request.params.temperature, Some(0.0));
        assert_eq!(
            request.messages.len(),
            1,
            "the prefix is flattened into one message"
        );
    }

    #[tokio::test]
    async fn what_the_user_said_reaches_the_summariser_whole() {
        let llm = StubProvider::answering("RECORD");
        let mut conversation = history();
        compactor(llm.clone(), 2)
            .compact("s1", &mut conversation)
            .await
            .unwrap();

        let transcript = llm.request().messages[0].text();
        assert!(
            transcript.contains("build a parser, and keep it dependency-free"),
            "the original request must survive: {transcript}"
        );
        assert!(transcript.contains("also handle CRLF"));
        assert!(
            transcript.contains("### tool call: read_file"),
            "tool activity must be visible: {transcript}"
        );
    }

    #[tokio::test]
    async fn bulky_tool_output_is_clipped_but_still_represented() {
        let llm = StubProvider::answering("RECORD");
        let mut conversation = history();
        compactor(llm.clone(), 2)
            .compact("s1", &mut conversation)
            .await
            .unwrap();

        let transcript = llm.request().messages[0].text();
        assert!(transcript.contains("### tool result: read_file [ok]"));
        assert!(
            transcript.len() < 10_000,
            "the 50 KB tool result must not be replayed in full: {} bytes",
            transcript.len()
        );
        assert!(transcript.contains('…'), "the clipping must be visible");
    }

    #[tokio::test]
    async fn a_second_compaction_carries_the_first_summary_forward() {
        let llm = StubProvider::answering("RECORD ONE");
        let mut conversation = history();
        compactor(llm, 2)
            .compact("s1", &mut conversation)
            .await
            .unwrap();

        for extra in ["a", "b", "c", "d"] {
            conversation.push(Message::assistant_text(extra));
        }

        let second = StubProvider::answering("RECORD TWO");
        compactor(second.clone(), 2)
            .compact("s1", &mut conversation)
            .await
            .unwrap();

        let transcript = second.request().messages[0].text();
        assert!(
            transcript.contains("RECORD ONE"),
            "the earlier record must be fed back in whole, or it is lost one \
             compaction later: {transcript}"
        );
    }

    #[test]
    fn a_blocks_share_of_the_transcript_halves_with_distance() {
        assert_eq!(block_budget(0), MAX_BLOCK_BYTES);
        assert_eq!(block_budget(HALVING_DISTANCE - 1), MAX_BLOCK_BYTES);
        assert_eq!(block_budget(HALVING_DISTANCE), MAX_BLOCK_BYTES / 2);
        assert_eq!(block_budget(HALVING_DISTANCE * 2), MAX_BLOCK_BYTES / 4);
        assert_eq!(
            block_budget(u64::MAX),
            MIN_BLOCK_BYTES,
            "the oldest turn is still described, never erased"
        );
    }

    #[tokio::test]
    async fn a_bulky_tail_is_folded_further_than_the_message_count_would() {
        // Twelve messages, which the count alone would protect in full - but
        // each is 20 KB and the tail is allowed 50.
        let mut conversation = Conversation::new();
        for _ in 0..12 {
            conversation.push(Message::assistant_text("y".repeat(20_000)));
        }
        assert_eq!(
            conversation.compaction_split(12),
            None,
            "by count alone this folds nothing, which is the bug"
        );

        let report = compactor_within(StubProvider::answering("RECORD"), 12, 50 * 1024)
            .compact("s1", &mut conversation)
            .await
            .expect("the byte cap must force the fold the count would not");

        assert_eq!(report.folded_messages, 10, "50 KB is room for two of them");
        assert_eq!(conversation.len(), 3, "the summary plus the two that fit");
        assert!(report.after_bytes < report.before_bytes, "{report:?}");
    }

    #[tokio::test]
    async fn the_transcript_thins_out_towards_the_bottom() {
        let llm = StubProvider::answering("RECORD");
        let mut conversation = Conversation::new();
        for index in 0..20 {
            conversation.push(Message::assistant_text(format!(
                "<{index:02}>{}",
                "x".repeat(3_000)
            )));
        }

        compactor(llm.clone(), 2)
            .compact("s1", &mut conversation)
            .await
            .unwrap();

        let transcript = llm.request().messages[0].text();
        let oldest = rendered_block(&transcript, "<00>");
        let newest = rendered_block(&transcript, "<17>");

        assert!(
            newest > oldest * 2,
            "detail must thin out with distance: <17> kept {newest} bytes, <00> kept {oldest}"
        );
        assert!(
            oldest >= MIN_BLOCK_BYTES,
            "and must never vanish altogether: {oldest}"
        );
    }

    /// Byte length of the rendered block the marker opens.
    fn rendered_block(transcript: &str, marker: &str) -> usize {
        let start = transcript
            .find(marker)
            .unwrap_or_else(|| panic!("{marker} never reached the summariser"));
        transcript[start..]
            .find('\n')
            .expect("a rendered block ends in a newline")
    }

    #[tokio::test]
    async fn a_distant_summary_is_still_carried_forward_whole() {
        // The failure this pins is a compaction quietly undoing the one before
        // it: the previous record is the oldest thing in the history, so a
        // thinning rule that did not exempt it would cut it first.
        let record = format!("RECORD ONE {}", "d".repeat(4_000));
        let mut conversation = history();
        compactor(StubProvider::answering(&record), 2)
            .compact("s1", &mut conversation)
            .await
            .unwrap();

        for index in 0..30 {
            conversation.push(Message::assistant_text(format!("filler {index}")));
        }

        let second = StubProvider::answering("RECORD TWO");
        compactor(second.clone(), 2)
            .compact("s1", &mut conversation)
            .await
            .unwrap();

        let transcript = second.request().messages[0].text();
        assert!(
            transcript.contains(&record),
            "the earlier record must survive whole, however far back it sits"
        );
    }

    #[tokio::test]
    async fn a_compaction_never_orphans_a_tool_result() {
        // keep_recent lands the cut on the tool result itself.
        let mut conversation = history();
        compactor(StubProvider::answering("RECORD"), 5)
            .compact("s1", &mut conversation)
            .await
            .unwrap();

        assert_ne!(
            conversation.messages()[1].role,
            Role::Tool,
            "history must not resume with a result whose call was folded away"
        );
    }
}
