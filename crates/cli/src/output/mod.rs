//! `agent run --output-format json|stream-json`: stdout as data.
//!
//! In these formats stdout carries JSON and nothing else - not the answer as
//! prose, not streamed fragments - so `agent run ... | jq` never meets a line
//! it cannot parse. Everything meant for a person (tool activity, warnings,
//! the Ctrl-C notice) stays on stderr, drawn by the same [`TerminalRenderer`]
//! as in `text`, minus the prose it would have put on stdout.
//!
//! `json` holds everything back and prints one result object when the turn
//! ends; `stream-json` prints one event per line as it happens, then the same
//! result object. The shapes are in [`dto`].

pub mod dto;

use std::io::Write;
use std::sync::{Arc, Mutex};

use agent_application::agent::AgentOutcome;
use agent_domain::model::llm::TokenUsage;
use agent_domain::ports::events::{AgentEvent, EventSink};
use agent_domain::text::is_unprintable;
use serde::Serialize;

use crate::args::OutputFormat;
use crate::render::TerminalRenderer;

use self::dto::{EventDto, ReasonDto, ResultDto, Versioned};

/// How a turn ended, as far as the result object is concerned.
#[derive(Debug)]
pub enum Ending<'a> {
    /// The loop returned - completed, out of iterations, stopped by the
    /// provider, or interrupted by one Ctrl-C.
    Finished(&'a AgentOutcome),
    /// The turn (or the agent's startup) failed with this message.
    Failed(String),
    /// Ctrl-C was pressed twice and the turn was dropped; only what the
    /// events showed is known.
    Abandoned,
}

/// The [`EventSink`] for the JSON formats, and the writer of their result.
pub struct JsonLinesSink {
    stream: bool,
    /// Where the events go for a person to read - stderr only.
    progress: Arc<dyn EventSink>,
    state: Mutex<State>,
}

struct State {
    out: Box<dyn Write + Send>,
    seen: Seen,
    /// The result is printed once: a second one would be a second "last
    /// line" for a reader to choose between.
    finished: bool,
}

/// What the events have shown so far: the result of a turn that has no
/// [`AgentOutcome`] (it failed, or was abandoned) is built from this.
#[derive(Default)]
struct Seen {
    session_id: Option<String>,
    iterations: u32,
    usage: TokenUsage,
    /// The last complete assistant message.
    message: String,
    /// Fragments streamed since it, for an answer cut off mid-stream.
    streamed: String,
}

impl Seen {
    fn record(&mut self, event: &AgentEvent) {
        match event {
            AgentEvent::RunStarted { session_id, .. } => {
                self.session_id = Some(session_id.clone());
            }
            AgentEvent::IterationStarted { iteration, .. } => {
                self.iterations = *iteration;
                self.streamed.clear();
            }
            AgentEvent::ModelResponded { usage, .. } => self.usage.accumulate(*usage),
            AgentEvent::AssistantDelta { text } => self.streamed.push_str(text),
            AgentEvent::AssistantMessage { text } => {
                self.message.clone_from(text);
                self.streamed.clear();
            }
            _ => {}
        }
    }

    fn partial_text(&self) -> String {
        if self.streamed.is_empty() {
            self.message.clone()
        } else {
            self.streamed.clone()
        }
    }
}

impl JsonLinesSink {
    /// The sink for `format`, writing to stdout and narrating to the
    /// terminal. `None` for [`OutputFormat::Text`], which is not JSON.
    pub fn stdout(format: OutputFormat, verbose: bool, no_color: bool) -> Option<Self> {
        Self::new(
            format,
            Arc::new(TerminalRenderer::new(verbose, no_color)),
            Box::new(std::io::stdout()),
        )
    }

    fn new(
        format: OutputFormat,
        progress: Arc<dyn EventSink>,
        out: Box<dyn Write + Send>,
    ) -> Option<Self> {
        let stream = match format {
            OutputFormat::Text => return None,
            OutputFormat::Json => false,
            OutputFormat::StreamJson => true,
        };
        Some(Self {
            stream,
            progress,
            state: Mutex::new(State {
                out,
                seen: Seen::default(),
                finished: false,
            }),
        })
    }

    /// Prints the result object - the last line of `stream-json`, the only
    /// one of `json`. Only the first call prints.
    pub fn finish(&self, ending: Ending<'_>, session_id: Option<&str>, exit_code: u8) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if state.finished {
            return;
        }
        state.finished = true;
        let seen = &state.seen;
        let session_id = session_id
            .map(str::to_string)
            .or_else(|| seen.session_id.clone());
        let result = match ending {
            Ending::Finished(outcome) => ResultDto {
                reason: (&outcome.reason).into(),
                final_text: outcome.final_text.clone(),
                session_id,
                iterations: outcome.iterations,
                usage: (&outcome.usage).into(),
                exit_code,
            },
            Ending::Failed(error) => ResultDto {
                reason: ReasonDto::Error { error },
                final_text: seen.partial_text(),
                session_id,
                iterations: seen.iterations,
                usage: (&seen.usage).into(),
                exit_code,
            },
            Ending::Abandoned => ResultDto {
                reason: ReasonDto::Interrupted,
                final_text: seen.partial_text(),
                session_id,
                iterations: seen.iterations,
                usage: (&seen.usage).into(),
                exit_code,
            },
        };
        write_line(&mut state.out, &Versioned::new(result));
    }
}

impl EventSink for JsonLinesSink {
    fn emit(&self, event: AgentEvent) {
        {
            let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            state.seen.record(&event);
            // Nothing after the result: it is the last line by contract.
            if self.stream && !state.finished {
                write_line(&mut state.out, &Versioned::new(EventDto::from(&event)));
            }
        }
        // The renderer puts the answer on stdout, which belongs to the JSON
        // here; the answer is in the result instead.
        if !matches!(
            event,
            AgentEvent::AssistantDelta { .. } | AgentEvent::AssistantMessage { .. }
        ) {
            self.progress.emit(event);
        }
    }
}

/// Writes `value` as one line and flushes it, so a reader sees each event as
/// it happens rather than when a buffer fills.
///
/// Strings are carried unchanged, but characters that a terminal would act on
/// (controls, bidirectional overrides, invisible ones) are written as
/// `\uXXXX` escapes. The parsed value is identical; only the raw bytes differ,
/// so `agent run --output-format json` printed straight to a terminal cannot
/// rewrite what is on it. serde_json already escapes the C0 controls; this
/// covers the rest of [`is_unprintable`].
///
/// A failed write (a closed pipe) is ignored, as the terminal renderer
/// ignores it: the reader is gone, and the turn's exit code still matters.
fn write_line(out: &mut dyn Write, value: &impl Serialize) {
    let mut line = Vec::new();
    let mut serializer = serde_json::Serializer::with_formatter(&mut line, TerminalSafe);
    if value.serialize(&mut serializer).is_err() {
        // Every DTO is plain data with string keys; serialising one cannot
        // fail. Printing half a line would be the worse outcome if it did.
        return;
    }
    line.push(b'\n');
    let _ = out.write_all(&line);
    let _ = out.flush();
}

/// serde_json's compact output, with [`is_unprintable`] characters escaped.
struct TerminalSafe;

impl serde_json::ser::Formatter for TerminalSafe {
    fn write_string_fragment<W>(&mut self, writer: &mut W, fragment: &str) -> std::io::Result<()>
    where
        W: ?Sized + Write,
    {
        let mut start = 0;
        for (index, c) in fragment.char_indices() {
            if is_unprintable(c) {
                writer.write_all(&fragment.as_bytes()[start..index])?;
                let mut units = [0u16; 2];
                for unit in c.encode_utf16(&mut units) {
                    write!(writer, "\\u{unit:04x}")?;
                }
                start = index + c.len_utf8();
            }
        }
        writer.write_all(&fragment.as_bytes()[start..])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_domain::ports::events::FinishReason;
    use serde_json::{Value, json};

    /// stdout, as the test sees it.
    #[derive(Clone, Default)]
    struct Captured(Arc<Mutex<Vec<u8>>>);

    impl Write for Captured {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl Captured {
        fn text(&self) -> String {
            String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
        }
        fn lines(&self) -> Vec<Value> {
            self.text()
                .lines()
                .map(|line| serde_json::from_str(line).expect("every line is JSON"))
                .collect()
        }
    }

    /// The events forwarded to the terminal renderer.
    #[derive(Default)]
    struct Progress(Mutex<Vec<AgentEvent>>);

    impl EventSink for Progress {
        fn emit(&self, event: AgentEvent) {
            self.0.lock().unwrap().push(event);
        }
    }

    fn sink(format: OutputFormat) -> (JsonLinesSink, Captured, Arc<Progress>) {
        let out = Captured::default();
        let progress = Arc::new(Progress::default());
        let sink = JsonLinesSink::new(format, progress.clone(), Box::new(out.clone()))
            .expect("a JSON format");
        (sink, out, progress)
    }

    fn turn(sink: &JsonLinesSink) {
        sink.emit(AgentEvent::IterationStarted {
            iteration: 1,
            limit: 5,
        });
        sink.emit(AgentEvent::AssistantDelta { text: "Hi".into() });
        sink.emit(AgentEvent::AssistantMessage { text: "Hi".into() });
    }

    fn outcome() -> AgentOutcome {
        AgentOutcome {
            final_text: "Hi".into(),
            iterations: 1,
            usage: TokenUsage::default(),
            reason: FinishReason::Completed,
        }
    }

    #[test]
    fn text_is_not_a_json_format() {
        assert!(
            JsonLinesSink::new(
                OutputFormat::Text,
                Arc::new(Progress::default()),
                Box::new(Captured::default())
            )
            .is_none()
        );
    }

    #[test]
    fn json_prints_nothing_until_the_result() {
        let (sink, out, _) = sink(OutputFormat::Json);
        turn(&sink);
        assert_eq!(out.text(), "", "no event and no prose before the end");

        sink.finish(Ending::Finished(&outcome()), Some("s-1"), 0);

        let lines = out.lines();
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert_eq!(lines[0]["type"], "result");
        assert_eq!(lines[0]["final_text"], "Hi");
        assert_eq!(lines[0]["session_id"], "s-1");
        assert_eq!(lines[0]["exit_code"], 0);
    }

    #[test]
    fn stream_json_prints_every_event_then_the_result_last() {
        let (sink, out, _) = sink(OutputFormat::StreamJson);
        turn(&sink);
        sink.finish(Ending::Finished(&outcome()), Some("s-1"), 0);
        // Late events (a sink shared past the turn) do not follow the result.
        sink.emit(AgentEvent::AssistantMessage {
            text: "late".into(),
        });

        let types: Vec<Value> = out.lines().iter().map(|l| l["type"].clone()).collect();
        assert_eq!(
            types,
            [
                "iteration_started",
                "assistant_delta",
                "assistant_message",
                "result"
            ]
        );
        assert!(out.lines().iter().all(|line| line["v"] == 1));
    }

    #[test]
    fn the_result_is_printed_once() {
        let (sink, out, _) = sink(OutputFormat::Json);
        sink.finish(Ending::Finished(&outcome()), Some("s-1"), 0);
        sink.finish(Ending::Failed("again".into()), Some("s-1"), 1);
        assert_eq!(out.lines().len(), 1);
    }

    #[test]
    fn prose_is_kept_from_the_terminal_renderer_and_the_rest_reaches_it() {
        let (sink, _, progress) = sink(OutputFormat::StreamJson);
        turn(&sink);
        let forwarded = progress.0.lock().unwrap();
        assert_eq!(
            forwarded.as_slice(),
            [AgentEvent::IterationStarted {
                iteration: 1,
                limit: 5
            }],
            "the renderer would print the prose on stdout"
        );
    }

    #[test]
    fn a_failed_turn_reports_what_the_events_showed() {
        let (sink, out, _) = sink(OutputFormat::Json);
        sink.emit(AgentEvent::RunStarted {
            session_id: "s-9".into(),
            provider: agent_domain::model::llm::ProviderId::new("local"),
            model: None,
        });
        sink.emit(AgentEvent::IterationStarted {
            iteration: 2,
            limit: 5,
        });
        sink.emit(AgentEvent::AssistantDelta {
            text: "half an ans".into(),
        });

        sink.finish(Ending::Failed("HTTP 500".into()), None, 1);

        assert_eq!(
            out.lines()[0],
            json!({"v": 1, "type": "result", "reason": "error", "error": "HTTP 500",
                   "final_text": "half an ans", "session_id": "s-9", "iterations": 2,
                   "usage": {"input_tokens": 0, "output_tokens": 0,
                             "cache_read_tokens": 0, "cache_write_tokens": 0},
                   "exit_code": 1})
        );
    }

    #[test]
    fn an_abandoned_turn_is_reported_as_interrupted() {
        let (sink, out, _) = sink(OutputFormat::Json);
        turn(&sink);
        sink.finish(Ending::Abandoned, Some("s-1"), 130);
        let result = &out.lines()[0];
        assert_eq!(result["reason"], "interrupted");
        assert_eq!(result["final_text"], "Hi");
        assert_eq!(result["exit_code"], 130);
    }

    #[test]
    fn model_strings_are_carried_unchanged_but_cannot_drive_a_terminal() {
        let (sink, out, _) = sink(OutputFormat::StreamJson);
        let hostile = "a\u{1b}[2Jb\u{202e}c\u{9b}d\n\"e\"";
        sink.emit(AgentEvent::AssistantMessage {
            text: hostile.into(),
        });

        let raw = out.text();
        assert_eq!(raw.lines().count(), 1, "one event, one line: {raw:?}");
        assert!(
            !raw.trim_end().chars().any(is_unprintable),
            "an unprintable character reached stdout raw: {raw:?}"
        );
        assert_eq!(out.lines()[0]["text"], hostile, "the value is unchanged");
    }
}
