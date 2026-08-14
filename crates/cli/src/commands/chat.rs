//! `agent chat` - an interactive session that keeps the conversation.

use agent_application::agent::Session;
use anyhow::Result;

use crate::args::Resume;
use crate::commands::tools;
use crate::composition::Application;
use crate::input::prompt_line;
use crate::session_id;

const HELP: &str = "  /reset  start a new conversation\n  \
                      /usage  tokens used so far\n  \
                      /tools  list the tools the model can call\n  \
                      /exit   quit";

/// What the REPL should do with a line before it reaches the model.
enum Input {
    Skip,
    Quit,
    Meta(&'static str),
    Prompt,
}

pub async fn execute(app: &Application, resume: Resume) -> Result<()> {
    banner(app);

    let mut session = start(app, resume).await;

    loop {
        let Some(line) = prompt_line("\n› ").await? else {
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
            Input::Prompt => {}
        }

        // A failed turn must not end the session: the user may want to fix the
        // configuration, or simply try a different prompt.
        if let Err(error) = app.agent.run(&mut session, line).await {
            eprintln!("error: {error:#}");
        }
    }

    Ok(())
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
            let mut session = Session::new(id);
            session.conversation = conversation;
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
        "/tools" => Input::Meta("tools"),
        _ => Input::Prompt,
    }
}

fn run_meta(app: &Application, session: &mut Session, command: &str) {
    match command {
        "help" => eprintln!("{HELP}"),
        "reset" => {
            *session = Session::new(session_id());
            eprintln!("  conversation cleared");
        }
        "usage" => eprintln!(
            "  {} in / {} out tokens over {} messages",
            session.usage.input_tokens,
            session.usage.output_tokens,
            session.turns()
        ),
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
    eprintln!("type /help for commands, /exit or Ctrl-D to quit");
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
