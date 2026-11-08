//! `agent chat` - an interactive session that keeps the conversation.
//!
//! Ctrl-C stops the running turn and comes back to the prompt with the
//! conversation intact; at the prompt it quits. See [`crate::ctrl_c`].

use std::process::ExitCode;

use agent_application::agent::Session;
use anyhow::Result;

use crate::args::Resume;
use crate::commands::turn::run_turn;
use crate::commands::{checkpoint, tools};
use crate::composition::Application;
use crate::ctrl_c::{CtrlC, Sigint, TurnEnd};
use crate::input::prompt_line;
use crate::notify::DRAIN_TIMEOUT;
use crate::render::{describe_plan, describe_usage};
use crate::session_id;

const HELP: &str = "  /reset  start a new conversation\n  \
                      /usage  tokens used so far\n  \
                      /plan   the model's current work plan\n  \
                      /undo   put back the files the last turn changed (the conversation stays)\n  \
                      /rewind [n]  take files and conversation back to before the last n turns\n  \
                      /tools  list the tools the model can call\n  \
                      /exit   quit\n  \
                      Ctrl-C  stop the running turn (at the prompt: quit)";

/// What the REPL should do with a line before it reaches the model.
enum Input {
    Skip,
    Quit,
    Meta(&'static str),
    Undo,
    /// `/rewind [n]`, or why its argument was not understood.
    Rewind(Result<usize, String>),
    Prompt,
}

pub async fn execute(app: &Application, resume: Resume) -> Result<ExitCode> {
    let result = repl(app, resume).await;
    // Every way out of the loop passes here - `/exit`, Ctrl-D, Ctrl-C and a
    // broken stdin - so the last turn's notification is not lost with the
    // process.
    app.notifications.drain(DRAIN_TIMEOUT).await;
    result
}

async fn repl(app: &Application, resume: Resume) -> Result<ExitCode> {
    let mut sigint = Sigint::listen()?;
    let mut keys = CtrlC::new();
    banner(app);

    let mut session = start(app, resume).await;

    loop {
        let line = tokio::select! {
            // Dropping the prompt here loses nothing typed: see `input`.
            () = sigint.recv() => match keys.pressed().exit_code() {
                Some(code) => {
                    eprintln!();
                    return Ok(code);
                }
                None => continue,
            },
            line = prompt_line("\n› ") => line?,
        };
        let Some(line) = line else {
            // Ctrl-D.
            eprintln!();
            break;
        };
        let line = line.trim();

        match classify(line) {
            Input::Skip => continue,
            Input::Quit => break,
            Input::Meta(command) => {
                run_meta(app, &mut session, command);
                continue;
            }
            Input::Undo => {
                checkpoint::undo(app, &mut session).await;
                continue;
            }
            Input::Rewind(Ok(turns)) => {
                checkpoint::rewind(app, &mut session, turns).await;
                continue;
            }
            Input::Rewind(Err(message)) => {
                eprintln!("  {message}");
                continue;
            }
            Input::Prompt => {}
        }

        // An interrupted turn is `Ok` and the session carries on from it.
        let result = match run_turn(app, &mut session, line, &mut sigint, &mut keys).await {
            TurnEnd::Finished(result) => result,
            TurnEnd::Abandoned(code) => return Ok(code),
        };

        // A failed turn must not end the session: the user may want to fix the
        // configuration, or simply try a different prompt.
        if let Err(error) = result {
            eprintln!("error: {error:#}");
        }
    }

    Ok(ExitCode::SUCCESS)
}

/// The session the REPL opens with.
///
/// Every way of failing to resume ends in a fresh session rather than in an
/// error. A missing record, an unreadable one, a `--resume` on a machine that
/// has never logged anything - none of those are reasons to refuse to start,
/// and someone who typed `--resume` out of habit should get a usable agent and
/// a line explaining why it is empty.
async fn start(app: &Application, resume: Resume) -> Session {
    let wanted = match resume {
        Resume::No => return Session::new(session_id()),
        Resume::Session(id) => Some(id),
        Resume::Latest => match app.sessions.latest().await {
            Ok(Some(id)) => Some(id),
            Ok(None) => {
                eprintln!("no session to resume; starting a new one");
                None
            }
            Err(error) => {
                eprintln!("cannot read the session directory ({error}); starting a new one");
                None
            }
        },
    };

    let Some(id) = wanted else {
        return Session::new(session_id());
    };

    match app.sessions.load(&id).await {
        Ok(conversation) if conversation.is_empty() => {
            eprintln!("session {id} has nothing in it; starting a new one");
            Session::new(session_id())
        }
        Ok(conversation) => {
            eprintln!("resuming {id} ({} messages)", conversation.len());
            // The same id, so the log carries on in the same file rather than
            // forking the record in two.
            let session = Session::resumed(id, conversation);
            if let Some(plan) = session.plan() {
                eprintln!(
                    "  with a plan of {} step(s), {} completed",
                    plan.items().len(),
                    plan.completed()
                );
            }
            session
        }
        Err(error) => {
            eprintln!("cannot resume {id} ({error}); starting a new one");
            Session::new(session_id())
        }
    }
}

fn classify(line: &str) -> Input {
    match line {
        "" => Input::Skip,
        "/exit" | "/quit" => Input::Quit,
        "/help" => Input::Meta("help"),
        "/reset" => Input::Meta("reset"),
        "/usage" => Input::Meta("usage"),
        "/plan" => Input::Meta("plan"),
        "/tools" => Input::Meta("tools"),
        "/undo" => Input::Undo,
        _ => match line.strip_prefix("/rewind") {
            Some(rest) if rest.is_empty() || rest.starts_with(' ') => {
                Input::Rewind(rewind_turns(rest.trim()))
            }
            _ => Input::Prompt,
        },
    }
}

fn rewind_turns(argument: &str) -> Result<usize, String> {
    if argument.is_empty() {
        return Ok(1);
    }
    match argument.parse::<usize>() {
        Ok(turns) if turns > 0 => Ok(turns),
        _ => Err(format!(
            "`/rewind` takes a number of turns, 1 or more (got `{}`)",
            agent_domain::text::printable(argument)
        )),
    }
}

fn run_meta(app: &Application, session: &mut Session, command: &str) {
    match command {
        "help" => eprintln!("{HELP}"),
        "reset" => {
            *session = Session::new(session_id());
            // Its turns belong to the conversation that just ended.
            app.checkpoints.clear();
            eprintln!("  conversation cleared");
        }
        "usage" => eprintln!(
            "  {} over {} messages",
            describe_usage(&session.usage),
            session.turns()
        ),
        "plan" => eprintln!("{}", describe_plan(session.plan())),
        "tools" => tools::execute(app),
        other => eprintln!("  unknown command `{other}`"),
    }
}

fn banner(app: &Application) {
    let provider = app.settings.default_provider_settings();
    eprintln!(
        "agent · {} · {} · workspace {} · approval {}",
        provider.kind.as_str(),
        provider.model,
        app.workspace.display(),
        app.settings.approval.as_str()
    );
    eprintln!("type /help for commands, /exit or Ctrl-D to quit, Ctrl-C to stop a turn");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blank_lines_are_ignored() {
        assert!(matches!(classify(""), Input::Skip));
    }

    #[test]
    fn slash_commands_are_recognised() {
        assert!(matches!(classify("/exit"), Input::Quit));
        assert!(matches!(classify("/quit"), Input::Quit));
        assert!(matches!(classify("/reset"), Input::Meta("reset")));
        assert!(matches!(classify("/tools"), Input::Meta("tools")));
        assert!(matches!(classify("/plan"), Input::Meta("plan")));
        assert!(matches!(classify("/undo"), Input::Undo));
    }

    #[test]
    fn rewind_takes_an_optional_positive_count() {
        assert!(matches!(classify("/rewind"), Input::Rewind(Ok(1))));
        assert!(matches!(classify("/rewind 3"), Input::Rewind(Ok(3))));
        assert!(matches!(classify("/rewind 0"), Input::Rewind(Err(_))));
        assert!(matches!(classify("/rewind two"), Input::Rewind(Err(_))));
        assert!(
            matches!(classify("/rewinding the tape"), Input::Prompt),
            "only the command itself, not a word that starts like it"
        );
    }

    #[test]
    fn anything_else_goes_to_the_model() {
        assert!(matches!(
            classify("what does src/main.rs do?"),
            Input::Prompt
        ));
        // A path that starts with a slash must not be mistaken for a command.
        assert!(matches!(
            classify("/etc/hosts is not readable, right?"),
            Input::Prompt
        ));
    }
}
