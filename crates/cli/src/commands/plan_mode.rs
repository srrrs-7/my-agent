//! `/plan-mode [on|off]` in `chat`: flip the switch and say what it means.
//!
//! The model is told by the application, in front of the next message; this
//! only tells the human. Everything goes to stderr, like every other meta
//! command.

use agent_application::agent::Session;
use agent_domain::text::printable;

use crate::composition::Application;

const COMMAND: &str = "/plan-mode";

/// Runs `line` if it is a `/plan-mode` command, and says whether it was.
pub(crate) fn handle(app: &Application, session: &mut Session, line: &str) -> bool {
    let Some(argument) = argument(line) else {
        return false;
    };
    let state = |on: bool| if on { "on" } else { "off" };
    match argument {
        "" => eprintln!("  plan mode is {}", state(app.plan_mode.is_on())),
        "on" | "off" => {
            let on = argument == "on";
            if !app.plan_mode.set(session, on) {
                eprintln!("  plan mode is already {}", state(on));
            } else if on {
                eprintln!(
                    "  plan mode on: the model can read and look things up, but nothing that \
                     writes files or runs commands will run until `/plan-mode off`"
                );
            } else {
                eprintln!("  plan mode off: tools that change things run again");
            }
        }
        other => eprintln!(
            "  `{COMMAND}` takes `on` or `off` (got `{}`)",
            printable(other)
        ),
    }
    true
}

/// The trimmed argument, or `None` when `line` is not this command - a
/// prompt that merely starts with the same letters goes to the model.
fn argument(line: &str) -> Option<&str> {
    let rest = line.strip_prefix(COMMAND)?;
    (rest.is_empty() || rest.starts_with(' ')).then(|| rest.trim())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_command_itself_is_taken() {
        assert_eq!(argument("/plan-mode"), Some(""));
        assert_eq!(argument("/plan-mode  on "), Some("on"));
        assert_eq!(argument("/plan-mode off"), Some("off"));
        assert_eq!(argument("/plan-modes are neat"), None);
        assert_eq!(argument("/plan"), None);
        assert_eq!(argument("please /plan-mode on"), None);
    }
}
