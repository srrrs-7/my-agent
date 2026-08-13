//! `agent chat` - an interactive session that keeps the conversation.

use agent_application::agent::Session;
use anyhow::Result;

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

pub async fn execute(app: &Application) -> Result<()> {
    banner(app);

    let mut session = Session::new(session_id());

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
