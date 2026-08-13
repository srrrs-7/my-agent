use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(
    name = "agent",
    version,
    about = "A sandboxed, loop-driven coding agent.",
    long_about = "Gives a language model a prompt, the workspace context and a set of file tools, \
                  then runs the tool-use loop until it produces an answer.\n\n\
                  Configuration is read from the environment (see .env.example); the flags below \
                  override it for a single invocation."
)]
pub struct Cli {
    /// Sandbox root. Every file tool is confined to this directory.
    #[arg(long, global = true, value_name = "DIR")]
    pub workspace: Option<PathBuf>,

    /// Model to use. With several providers configured, `provider/model`
    /// selects one, e.g. `cloud/claude-sonnet-5`.
    #[arg(long, short = 'm', global = true, value_name = "MODEL")]
    pub model: Option<String>,

    /// Approve every tool call without asking.
    #[arg(long, short = 'y', global = true)]
    pub yes: bool,

    /// Maximum number of model round-trips per turn.
    #[arg(long, global = true, value_name = "N")]
    pub max_iterations: Option<u32>,

    /// Show model, token usage and latency for every round-trip.
    #[arg(long, short = 'v', global = true)]
    pub verbose: bool,

    /// Disable ANSI colour.
    #[arg(long, global = true)]
    pub no_color: bool,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Answer a single prompt and exit.
    Run {
        /// The prompt. Quoting is optional; all words are joined.
        #[arg(required = true, trailing_var_arg = true)]
        prompt: Vec<String>,
    },

    /// Interactive session that keeps the conversation history.
    Chat,

    /// List the tools exposed to the model.
    Tools,

    /// Show the resolved configuration and check that the LLM answers.
    Doctor,
}

impl Cli {
    /// The subcommand to execute. Named `resolve_command` rather than
    /// `command` so it does not shadow `clap::CommandFactory::command`.
    pub fn resolve_command(&self) -> Command {
        match &self.command {
            Some(Command::Run { prompt }) => Command::Run {
                prompt: prompt.clone(),
            },
            Some(Command::Chat) => Command::Chat,
            Some(Command::Tools) => Command::Tools,
            Some(Command::Doctor) => Command::Doctor,
            // Bare `agent` drops into the REPL, like `psql` or `irb`.
            None => Command::Chat,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn joins_a_multi_word_prompt() {
        let cli = Cli::parse_from(["agent", "run", "list", "the", "files"]);
        match cli.resolve_command() {
            Command::Run { prompt } => assert_eq!(prompt.join(" "), "list the files"),
            other => panic!("expected run, got {other:?}"),
        }
    }

    #[test]
    fn defaults_to_chat() {
        assert!(matches!(
            Cli::parse_from(["agent"]).resolve_command(),
            Command::Chat
        ));
    }

    #[test]
    fn global_flags_work_after_the_subcommand() {
        let cli = Cli::parse_from(["agent", "--yes", "-m", "cloud/claude-sonnet-5", "chat"]);
        assert!(cli.yes);
        assert_eq!(cli.model.as_deref(), Some("cloud/claude-sonnet-5"));
    }
}
