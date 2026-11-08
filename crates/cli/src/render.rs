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
use agent_domain::text::printable;

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

    /// `text`, made [`printable`] and coloured.
    ///
    /// Every piece of a status line goes through here - or through
    /// [`printable`] directly when it is not coloured - so the sanitising
    /// happens where text leaves for the terminal, and nothing upstream has to
    /// remember to do it. The escape codes of the colour itself are added
    /// after, which is why this cannot be applied to a finished line.
    fn paint(&self, style: &str, text: &str) -> String {
        let text = printable(text);
        if self.color {
            format!("{style}{text}{RESET}")
        } else {
            text
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
        match &event {
            AgentEvent::AssistantDelta { text } => return self.answer_delta(text),
            AgentEvent::AssistantMessage { text } => return self.answer(text),
            // A turn that failed mid-stream never got its `AssistantMessage`
            // or its `RunFinished`; without this its half line would run into
            // the next turn's output, and the next turn's answer - if it is
            // not streamed - would be taken for the end of it and swallowed.
            AgentEvent::RunStarted { .. }
            | AgentEvent::IterationStarted { .. }
            | AgentEvent::RunFinished { .. } => self.end_stream(),
            _ => {}
        }
        if let Some(line) = self.status_line(&event) {
            self.status(line);
        }
    }
}

impl TerminalRenderer {
    /// What `event` prints on stderr, if anything. Separate from the writing
    /// so that what reaches the terminal can be tested.
    fn status_line(&self, event: &AgentEvent) -> Option<String> {
        let line = match event {
            AgentEvent::RunStarted {
                provider, model, ..
            } => {
                if !self.verbose {
                    return None;
                }
                let model = model
                    .as_ref()
                    .map(|m| m.to_string())
                    .unwrap_or_else(|| "default".into());
                self.paint(DIM, &format!("· provider={provider} model={model}"))
            }

            AgentEvent::IterationStarted { iteration, limit } => {
                if !self.verbose {
                    return None;
                }
                self.paint(DIM, &format!("· iteration {iteration}/{limit}"))
            }

            AgentEvent::HistoryTrimmed { dropped_messages } => self.paint(
                YELLOW,
                &format!("! trimmed {dropped_messages} old messages to fit the context budget"),
            ),

            AgentEvent::HistoryCompacted {
                folded_messages,
                before_bytes,
                after_bytes,
            } => {
                // Deliberately not a warning: unlike a trim, nothing was lost.
                self.paint(
                    DIM,
                    &format!(
                        "~ compacted {folded_messages} old messages into a summary \
                         ({} KB -> {} KB)",
                        before_bytes / 1024,
                        after_bytes / 1024
                    ),
                )
            }

            AgentEvent::ModelResponded {
                model,
                usage,
                latency,
                stop_reason,
                ..
            } => {
                if !self.verbose {
                    return None;
                }
                self.paint(
                    DIM,
                    &format!(
                        "· {model} · {} · {} · {stop_reason:?}",
                        describe_usage(usage),
                        format_duration(*latency),
                    ),
                )
            }

            AgentEvent::AssistantDelta { .. } | AgentEvent::AssistantMessage { .. } => {
                return None;
            }

            AgentEvent::ToolCallStarted { call, safety } => {
                let marker = if safety.is_read_only() { CYAN } else { YELLOW };
                format!(
                    "{} {}",
                    self.paint(marker, "→"),
                    self.paint(BOLD, call.name.as_str())
                )
            }

            AgentEvent::ToolCallFinished {
                name,
                is_error,
                summary,
                duration,
                ..
            } => {
                let (style, glyph) = if *is_error {
                    (RED, "✗")
                } else {
                    (GREEN, "✓")
                };
                format!(
                    "{} {} {}",
                    self.paint(style, glyph),
                    self.paint(DIM, &format!("{name} ({})", format_duration(*duration))),
                    printable(summary)
                )
            }

            AgentEvent::ToolCallDenied { name, reason } => {
                self.paint(YELLOW, &format!("⊘ {name} denied: {reason}"))
            }

            AgentEvent::PlanUpdated { plan } => self.checklist(plan),

            AgentEvent::DependenciesChanged { name, files } => {
                self.paint(YELLOW, &describe_dependency_files(&name.to_string(), files))
            }

            AgentEvent::DependencyCheckIncomplete { name, limit } => self.paint(
                YELLOW,
                &format!(
                    "! {name}: dependency check incomplete - the workspace scan stopped \
                     after {limit} entries, so a dependency change may have gone unreported"
                ),
            ),

            AgentEvent::RunFinished {
                reason,
                iterations,
                usage,
            } => return self.run_finished(reason, *iterations, usage),
        };
        Some(line)
    }

    fn run_finished(
        &self,
        reason: &FinishReason,
        iterations: u32,
        usage: &TokenUsage,
    ) -> Option<String> {
        let line = match reason {
            FinishReason::Completed => {
                if !self.verbose {
                    return None;
                }
                self.paint(
                    DIM,
                    &format!(
                        "· done in {iterations} iteration(s), {} tokens{}",
                        usage.total(),
                        cache_suffix(usage)
                    ),
                )
            }
            FinishReason::MaxIterations { limit } => self.paint(
                YELLOW,
                &format!(
                    "! stopped after the {limit}-iteration budget; the answer above may \
                     be incomplete (raise --max-iterations to allow more)"
                ),
            ),
            FinishReason::Stopped { stop_reason } => {
                self.paint(YELLOW, &format!("! the model stopped: {stop_reason}"))
            }
            FinishReason::Interrupted => self.paint(YELLOW, "! interrupted"),
        };
        Some(line)
    }
}

/// Files listed in a dependency warning before the rest are counted.
const MAX_FILES_SHOWN: usize = 10;

/// The warning for dependency files a call changed without approval. Not yet
/// [`printable`]: the caller paints it, which cleans it.
fn describe_dependency_files(tool: &str, files: &[DependencyFileChange]) -> String {
    let mut shown: Vec<String> = files
        .iter()
        .take(MAX_FILES_SHOWN)
        .map(|file| {
            format!(
                "{} ({} {})",
                file.path,
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

/// The plan as a checklist, one item per line, the step in progress
/// highlighted. Always the whole plan: the event only fires when it changed,
/// and a diff would leave the reader to assemble it.
///
/// `paint` must make what it is given [`printable`]. [`Plan::new`] already
/// refuses an item that is not, but the line breaks between items are the only
/// ones this output may contain, so the items are cleaned here regardless.
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
            PlanStatus::Pending => format!("  [ ] {}", printable(&item.content)),
        });
    }
    lines.join("\n")
}

/// The session's plan for `/plan`, uncoloured, in the same shape the renderer
/// draws it in.
pub fn describe_plan(plan: Option<&Plan>) -> String {
    match plan {
        Some(plan) => draw_plan(plan, &|_, text| printable(text)),
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
    use agent_domain::model::workspace::WorkspacePath;

    #[test]
    fn a_dependency_warning_names_the_tool_and_every_file_up_to_a_limit() {
        use agent_domain::model::dependency::{FileChange, ManifestKind};
        let files: Vec<DependencyFileChange> = (0..MAX_FILES_SHOWN + 2)
            .map(|n| DependencyFileChange {
                path: WorkspacePath::parse(&format!("crate{n}/Cargo.lock")).unwrap(),
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

    #[test]
    fn a_new_turn_ends_an_answer_a_failed_turn_left_streaming() {
        let renderer = TerminalRenderer::new(false, true);
        renderer.emit(AgentEvent::AssistantDelta {
            text: String::new(),
        });
        // The stream failed: no `AssistantMessage`, no `RunFinished`.
        renderer.emit(AgentEvent::RunStarted {
            session_id: "s".into(),
            provider: agent_domain::model::llm::ProviderId::new("p"),
            model: None,
        });
        assert!(
            !renderer.state.lock().unwrap().streaming,
            "the next answer would be printed as an empty line"
        );

        renderer.emit(AgentEvent::AssistantDelta {
            text: String::new(),
        });
        renderer.emit(AgentEvent::IterationStarted {
            iteration: 2,
            limit: 3,
        });
        assert!(!renderer.state.lock().unwrap().streaming);
    }

    /// Every stderr line an event can produce, fed the worst a model can
    /// write, carries no character that acts on the terminal - only the line
    /// breaks of a multi-line checklist.
    #[test]
    fn no_status_line_lets_a_control_character_through() {
        use agent_domain::model::dependency::{FileChange, ManifestKind};
        use agent_domain::model::llm::{ModelId, ProviderId, StopReason};
        use agent_domain::model::tool::{ToolCall, ToolCallId, ToolName, ToolSafety};

        const EVIL: &str = "x\r\x1b[2Kok\u{202E}\u{9B}\t\u{200B}";
        let renderer = TerminalRenderer::new(true, true);
        let name = ToolName::new("run_command").unwrap();
        let plan = Plan::new(vec![
            PlanItem::new("done", PlanStatus::Completed),
            PlanItem::new("doing", PlanStatus::InProgress),
            PlanItem::new("next", PlanStatus::Pending),
        ])
        .unwrap();
        let events = vec![
            AgentEvent::RunStarted {
                session_id: EVIL.into(),
                provider: ProviderId::new("local"),
                model: Some(ModelId::new(EVIL)),
            },
            AgentEvent::ModelResponded {
                provider: ProviderId::new("local"),
                model: ModelId::new(EVIL),
                stop_reason: StopReason::Other(EVIL.into()),
                usage: TokenUsage::default(),
                latency: Duration::from_millis(1),
            },
            AgentEvent::ToolCallStarted {
                call: ToolCall::new(ToolCallId::new(EVIL), name.clone(), serde_json::json!(EVIL)),
                safety: ToolSafety::Destructive,
            },
            AgentEvent::ToolCallFinished {
                name: name.clone(),
                call_id: ToolCallId::new(EVIL),
                is_error: true,
                summary: EVIL.into(),
                duration: Duration::from_millis(1),
            },
            AgentEvent::ToolCallDenied {
                name: name.clone(),
                reason: EVIL.into(),
            },
            AgentEvent::PlanUpdated { plan },
            AgentEvent::DependenciesChanged {
                name: name.clone(),
                files: vec![DependencyFileChange {
                    path: WorkspacePath::parse(EVIL).unwrap(),
                    kind: ManifestKind::CargoToml,
                    change: FileChange::Modified,
                }],
            },
            AgentEvent::RunFinished {
                reason: FinishReason::Stopped {
                    stop_reason: EVIL.into(),
                },
                iterations: 1,
                usage: TokenUsage::default(),
            },
        ];

        for event in events {
            let label = format!("{event:?}");
            let line = renderer
                .status_line(&event)
                .unwrap_or_else(|| panic!("verbose prints every one of these: {label}"));
            assert!(
                !line
                    .chars()
                    .any(|c| c != '\n' && agent_domain::text::is_unprintable(c)),
                "{line:?} from {label}"
            );
        }
    }

    #[test]
    fn the_plan_command_shows_an_item_without_its_control_characters() {
        // `Plan::new` refuses these, so a plan can only carry one if that
        // check is ever loosened; the output must not depend on it.
        let plan = Plan::new(vec![PlanItem::new("ok", PlanStatus::Pending)]).unwrap();
        let shown = draw_plan(&plan, &|_, text| printable(text));
        assert_eq!(shown, "☐ plan (0/1 completed)\n  [ ] ok");
        assert_eq!(
            printable("  [ ] a\x1b[2Kb"),
            "  [ ] a?[2Kb",
            "what `/plan` passes each line through"
        );
    }
}
