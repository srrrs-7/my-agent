//! Terminal rendering of [`AgentEvent`]s.
//!
//! Channel discipline: the model's prose goes to **stdout**, everything else
//! (tool activity, usage, warnings) goes to **stderr**. That way
//! `agent run "..." > answer.md` captures the answer and nothing else.

use std::io::{IsTerminal, Write};
use std::sync::Mutex;
use std::time::Duration;

use agent_domain::model::dependency::DependencyFileChange;
use agent_domain::model::llm::TokenUsage;
use agent_domain::model::plan::{Plan, PlanStatus};
use agent_domain::ports::events::{AgentEvent, EventSink, FinishReason};

const RESET: &str = "\x1b[0m";
const DIM: &str = "\x1b[2m";
const BOLD: &str = "\x1b[1m";
const CYAN: &str = "\x1b[36m";
const GREEN: &str = "\x1b[32m";
const YELLOW: &str = "\x1b[33m";
const RED: &str = "\x1b[31m";

pub struct TerminalRenderer {
    verbose: bool,
    color: bool,
    /// Serialises writes so concurrent read-only tool calls cannot interleave
    /// their lines, and remembers whether the current answer has already been
    /// streamed to stdout as deltas.
    state: Mutex<RenderState>,
}

#[derive(Default)]
struct RenderState {
    /// True once an `AssistantDelta` was printed for the answer in progress.
    /// The full `AssistantMessage` then only terminates the line - printing it
    /// again would duplicate everything the deltas already showed.
    streaming: bool,
}

impl TerminalRenderer {
    pub fn new(verbose: bool, no_color: bool) -> Self {
        let color =
            !no_color && std::io::stderr().is_terminal() && std::env::var_os("NO_COLOR").is_none();
        Self {
            verbose,
            color,
            state: Mutex::new(RenderState::default()),
        }
    }

    fn paint(&self, style: &str, text: &str) -> String {
        if self.color {
            format!("{style}{text}{RESET}")
        } else {
            text.to_string()
        }
    }

    fn status(&self, line: impl AsRef<str>) {
        let _guard = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let mut stderr = std::io::stderr();
        let _ = writeln!(stderr, "{}", line.as_ref());
        let _ = stderr.flush();
    }

    fn checklist(&self, plan: &Plan) -> String {
        draw_plan(plan, &|style, text| self.paint(style, text))
    }

    /// Prints one streamed fragment, without a newline, flushed immediately.
    fn answer_delta(&self, text: &str) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        state.streaming = true;
        let mut stdout = std::io::stdout();
        let _ = write!(stdout, "{text}");
        let _ = stdout.flush();
    }

    /// Ends a streamed answer that will never get its `AssistantMessage` - the
    /// turn was interrupted mid-stream - so the next line does not start
    /// after half a sentence, and the next turn's answer is not mistaken for
    /// its continuation.
    fn end_stream(&self) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if state.streaming {
            state.streaming = false;
            let mut stdout = std::io::stdout();
            let _ = writeln!(stdout);
            let _ = stdout.flush();
        }
    }

    fn answer(&self, text: &str) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let mut stdout = std::io::stdout();
        if state.streaming {
            // The prose is already on screen; just end the line.
            state.streaming = false;
            let _ = writeln!(stdout);
        } else {
            let _ = writeln!(stdout, "{text}");
        }
        let _ = stdout.flush();
    }
}

impl EventSink for TerminalRenderer {
    fn emit(&self, event: AgentEvent) {
        match event {
            AgentEvent::RunStarted {
                provider, model, ..
            } => {
                if self.verbose {
                    let model = model
                        .map(|m| m.to_string())
                        .unwrap_or_else(|| "default".into());
                    self.status(self.paint(DIM, &format!("· provider={provider} model={model}")));
                }
            }

            AgentEvent::IterationStarted { iteration, limit } => {
                if self.verbose {
                    self.status(self.paint(DIM, &format!("· iteration {iteration}/{limit}")));
                }
            }

            AgentEvent::HistoryTrimmed { dropped_messages } => {
                self.status(self.paint(
                    YELLOW,
                    &format!("! trimmed {dropped_messages} old messages to fit the context budget"),
                ));
            }

            AgentEvent::HistoryCompacted {
                folded_messages,
                before_bytes,
                after_bytes,
            } => {
                // Deliberately not a warning: unlike a trim, nothing was lost.
                self.status(self.paint(
                    DIM,
                    &format!(
                        "~ compacted {folded_messages} old messages into a summary \
                         ({} KB -> {} KB)",
                        before_bytes / 1024,
                        after_bytes / 1024
                    ),
                ));
            }

            AgentEvent::ModelResponded {
                model,
                usage,
                latency,
                stop_reason,
                ..
            } => {
                if self.verbose {
                    self.status(self.paint(
                        DIM,
                        &format!(
                            "· {model} · {} · {} · {stop_reason:?}",
                            describe_usage(&usage),
                            format_duration(latency),
                        ),
                    ));
                }
            }

            AgentEvent::AssistantDelta { text } => self.answer_delta(&text),

            AgentEvent::AssistantMessage { text } => self.answer(&text),

            AgentEvent::ToolCallStarted { call, safety } => {
                let marker = if safety.is_read_only() { CYAN } else { YELLOW };
                self.status(format!(
                    "{} {}",
                    self.paint(marker, "→"),
                    self.paint(BOLD, call.name.as_str())
                ));
            }

            AgentEvent::ToolCallFinished {
                name,
                is_error,
                summary,
                duration,
                ..
            } => {
                let (style, glyph) = if is_error {
                    (RED, "✗")
                } else {
                    (GREEN, "✓")
                };
                self.status(format!(
                    "{} {} {}",
                    self.paint(style, glyph),
                    self.paint(DIM, &format!("{name} ({})", format_duration(duration))),
                    summary
                ));
            }

            AgentEvent::ToolCallDenied { name, reason } => {
                self.status(self.paint(YELLOW, &format!("⊘ {name} denied: {reason}")));
            }

            AgentEvent::PlanUpdated { plan } => self.status(self.checklist(&plan)),

            AgentEvent::DependenciesChanged { name, files } => {
                self.status(self.paint(
                    YELLOW,
                    &describe_dependency_files(&name.to_string(), &files),
                ));
            }

            AgentEvent::DependencyCheckIncomplete { name, limit } => {
                self.status(self.paint(
                    YELLOW,
                    &format!(
                        "! {name}: dependency check incomplete - the workspace scan stopped \
                         after {limit} entries, so a dependency change may have gone unreported"
                    ),
                ));
            }

            AgentEvent::RunFinished {
                reason,
                iterations,
                usage,
            } => {
                self.end_stream();
                self.run_finished(reason, iterations, usage);
            }
        }
    }
}

impl TerminalRenderer {
    fn run_finished(&self, reason: FinishReason, iterations: u32, usage: TokenUsage) {
        match reason {
            FinishReason::Completed => {
                if self.verbose {
                    self.status(self.paint(
                        DIM,
                        &format!(
                            "· done in {iterations} iteration(s), {} tokens{}",
                            usage.total(),
                            cache_suffix(&usage)
                        ),
                    ));
                }
            }
            FinishReason::MaxIterations { limit } => {
                self.status(self.paint(
                    YELLOW,
                    &format!(
                        "! stopped after the {limit}-iteration budget; the answer above may \
                             be incomplete (raise --max-iterations to allow more)"
                    ),
                ));
            }
            FinishReason::Stopped { stop_reason } => {
                self.status(self.paint(YELLOW, &format!("! the model stopped: {stop_reason}")));
            }
            FinishReason::Interrupted => {
                self.status(self.paint(YELLOW, "! interrupted"));
            }
        }
    }
}

/// Files listed in a dependency warning before the rest are counted.
const MAX_FILES_SHOWN: usize = 10;

/// The warning for dependency files a call changed without approval.
fn describe_dependency_files(tool: &str, files: &[DependencyFileChange]) -> String {
    let mut shown: Vec<String> = files
        .iter()
        .take(MAX_FILES_SHOWN)
        .map(|file| {
            format!(
                "{} ({} {})",
                printable(&file.path),
                file.kind.label(),
                file.change.label()
            )
        })
        .collect();
    if files.len() > MAX_FILES_SHOWN {
        shown.push(format!("and {} more", files.len() - MAX_FILES_SHOWN));
    }
    format!(
        "! {tool} changed dependency files without approval: {}",
        shown.join(", ")
    )
}

/// `text` with everything that could act on the terminal instead of being
/// shown replaced by `?`: control characters (escape sequences, carriage
/// returns that redraw a line, newlines that push the question off screen)
/// and the bidirectional overrides that make text read differently from what
/// it is. Used for what came from the model or from a file, wherever a human
/// decides on the strength of it.
pub fn printable(text: &str) -> String {
    text.chars()
        .map(|c| {
            let bidi = matches!(c, '\u{200E}' | '\u{200F}' | '\u{061C}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}');
            if c.is_control() || bidi { '?' } else { c }
        })
        .collect()
}

/// The plan as a checklist, one item per line, the step in progress
/// highlighted. Always the whole plan: the event only fires when it changed,
/// and a diff would leave the reader to assemble it.
fn draw_plan(plan: &Plan, paint: &dyn Fn(&'static str, &str) -> String) -> String {
    if plan.is_empty() {
        return paint(DIM, "☐ plan cleared");
    }
    let mut lines = vec![paint(
        BOLD,
        &format!(
            "☐ plan ({}/{} completed)",
            plan.completed(),
            plan.items().len()
        ),
    )];
    for item in plan.items() {
        lines.push(match item.status {
            PlanStatus::Completed => paint(DIM, &format!("  [x] {}", item.content)),
            PlanStatus::InProgress => paint(CYAN, &format!("  [>] {}", item.content)),
            PlanStatus::Pending => format!("  [ ] {}", item.content),
        });
    }
    lines.join("\n")
}

/// The session's plan for `/plan`, uncoloured, in the same shape the renderer
/// draws it in.
pub fn describe_plan(plan: Option<&Plan>) -> String {
    match plan {
        Some(plan) => draw_plan(plan, &|_, text| text.to_string()),
        None => "no plan".to_string(),
    }
}

/// `1200 in / 40 out tokens`, followed by what the prompt cache did whenever
/// it did anything. Shared by `-v` and `/usage` so the two never disagree.
pub fn describe_usage(usage: &TokenUsage) -> String {
    format!(
        "{} in / {} out tokens{}",
        usage.input_tokens,
        usage.output_tokens,
        cache_suffix(usage)
    )
}

/// Empty when the cache was not involved - every request to a server without
/// one, whose usage line should read the same as it always has.
fn cache_suffix(usage: &TokenUsage) -> String {
    if usage.touched_cache() {
        format!(
            " (cache: {} read, {} written)",
            usage.cache_read_tokens, usage.cache_write_tokens
        )
    } else {
        String::new()
    }
}

fn format_duration(duration: Duration) -> String {
    let millis = duration.as_millis();
    if millis < 1000 {
        format!("{millis}ms")
    } else {
        format!("{:.1}s", duration.as_secs_f64())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_domain::model::plan::PlanItem;

    #[test]
    fn nothing_that_drives_the_terminal_survives_printable() {
        let shown = printable("evil\x1b[2J\r\nsafe\u{202E}gnp.exe");
        assert_eq!(shown, "evil?[2J??safe?gnp.exe");
    }

    #[test]
    fn a_dependency_warning_names_the_tool_and_every_file_up_to_a_limit() {
        use agent_domain::model::dependency::{FileChange, ManifestKind};
        let files: Vec<DependencyFileChange> = (0..MAX_FILES_SHOWN + 2)
            .map(|n| DependencyFileChange {
                path: format!("crate{n}/Cargo.lock"),
                kind: ManifestKind::CargoLock,
                change: FileChange::Modified,
            })
            .collect();
        let shown = describe_dependency_files("run_command", &files);
        assert!(
            shown.starts_with("! run_command changed dependency files"),
            "{shown}"
        );
        assert!(
            shown.contains("crate0/Cargo.lock (lockfile modified)"),
            "{shown}"
        );
        assert!(shown.ends_with("and 2 more"), "{shown}");
    }

    #[test]
    fn durations_switch_units() {
        assert_eq!(format_duration(Duration::from_millis(42)), "42ms");
        assert_eq!(format_duration(Duration::from_millis(1500)), "1.5s");
    }

    #[test]
    fn usage_mentions_the_cache_only_when_it_was_used() {
        let uncached = TokenUsage {
            input_tokens: 120,
            output_tokens: 24,
            ..TokenUsage::default()
        };
        assert_eq!(describe_usage(&uncached), "120 in / 24 out tokens");

        let cached = TokenUsage {
            input_tokens: 4_050,
            output_tokens: 7,
            cache_read_tokens: 3_000,
            cache_write_tokens: 1_000,
        };
        assert_eq!(
            describe_usage(&cached),
            "4050 in / 7 out tokens (cache: 3000 read, 1000 written)"
        );
    }

    #[test]
    fn a_plan_is_drawn_as_a_checklist_with_progress() {
        let renderer = TerminalRenderer::new(false, true);
        let plan = Plan::new(vec![
            PlanItem::new("read the parser", PlanStatus::Completed),
            PlanItem::new("handle CRLF", PlanStatus::InProgress),
            PlanItem::new("run the tests", PlanStatus::Pending),
        ])
        .unwrap();
        assert_eq!(
            renderer.checklist(&plan),
            "☐ plan (1/3 completed)\n  [x] read the parser\n  [>] handle CRLF\n  [ ] run the tests"
        );
        assert_eq!(renderer.checklist(&Plan::default()), "☐ plan cleared");
        assert_eq!(describe_plan(Some(&plan)), renderer.checklist(&plan));
        assert_eq!(describe_plan(None), "no plan");
    }

    #[test]
    fn a_finished_run_ends_an_answer_left_streaming() {
        let renderer = TerminalRenderer::new(false, true);
        // Empty, so the test prints nothing but the line ending.
        renderer.emit(AgentEvent::AssistantDelta {
            text: String::new(),
        });
        assert!(renderer.state.lock().unwrap().streaming);

        // Interrupted mid-stream: no `AssistantMessage` is coming.
        renderer.emit(AgentEvent::RunFinished {
            reason: FinishReason::Interrupted,
            iterations: 1,
            usage: TokenUsage::default(),
        });

        assert!(
            !renderer.state.lock().unwrap().streaming,
            "the next turn's answer would be swallowed as a continuation"
        );
    }

    #[test]
    fn colour_is_suppressed_on_request() {
        let renderer = TerminalRenderer::new(false, true);
        assert_eq!(renderer.paint(RED, "x"), "x");
    }
}
