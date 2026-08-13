//! Terminal rendering of [`AgentEvent`]s.
//!
//! Channel discipline: the model's prose goes to **stdout**, everything else
//! (tool activity, usage, warnings) goes to **stderr**. That way
//! `agent run "..." > answer.md` captures the answer and nothing else.

use std::io::{IsTerminal, Write};
use std::sync::Mutex;
use std::time::Duration;

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
    /// their lines.
    lock: Mutex<()>,
}

impl TerminalRenderer {
    pub fn new(verbose: bool, no_color: bool) -> Self {
        let color =
            !no_color && std::io::stderr().is_terminal() && std::env::var_os("NO_COLOR").is_none();
        Self {
            verbose,
            color,
            lock: Mutex::new(()),
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
        let _guard = self.lock.lock().unwrap_or_else(|error| error.into_inner());
        let mut stderr = std::io::stderr();
        let _ = writeln!(stderr, "{}", line.as_ref());
        let _ = stderr.flush();
    }

    fn answer(&self, text: &str) {
        let _guard = self.lock.lock().unwrap_or_else(|error| error.into_inner());
        let mut stdout = std::io::stdout();
        let _ = writeln!(stdout, "{text}");
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
                            "· {model} · {} in / {} out tokens · {} · {stop_reason:?}",
                            usage.input_tokens,
                            usage.output_tokens,
                            format_duration(latency),
                        ),
                    ));
                }
            }

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

            AgentEvent::RunFinished {
                reason,
                iterations,
                usage,
            } => match reason {
                FinishReason::Completed => {
                    if self.verbose {
                        self.status(self.paint(
                            DIM,
                            &format!(
                                "· done in {iterations} iteration(s), {} tokens",
                                usage.total()
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
            },
        }
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

    #[test]
    fn durations_switch_units() {
        assert_eq!(format_duration(Duration::from_millis(42)), "42ms");
        assert_eq!(format_duration(Duration::from_millis(1500)), "1.5s");
    }

    #[test]
    fn colour_is_suppressed_on_request() {
        let renderer = TerminalRenderer::new(false, true);
        assert_eq!(renderer.paint(RED, "x"), "x");
    }
}
