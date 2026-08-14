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

    /// Replace the built-in system prompt with this file's contents
    /// (overrides AGENT_SYSTEM_PROMPT_FILE).
    #[arg(long, global = true, value_name = "FILE")]
    pub system_prompt_file: Option<PathBuf>,

    /// Append extra instructions to the end of the system prompt
    /// (overrides AGENT_APPEND_SYSTEM_PROMPT).
    #[arg(long, global = true, value_name = "TEXT")]
    pub append_system_prompt: Option<String>,

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
    Chat {
        /// Continue a previous session. Without an ID, the most recent one.
        ///
        /// Reads the session log, so it needs AGENT_SESSION_LOG to have been
        /// on when that session ran.
        #[arg(long, value_name = "ID", num_args = 0..=1, default_missing_value = "")]
        resume: Option<String>,
    },

    /// List the saved sessions.
    Sessions {
        /// Delete this session's record instead of listing.
        #[arg(long, value_name = "ID")]
        delete: Option<String>,
    },

    /// List the tools exposed to the model.
    Tools,

    /// Show the resolved configuration and check that the LLM answers.
    Doctor,
}

/// Which conversation `agent chat` should start from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resume {
    /// A fresh conversation.
    No,
    /// Whichever session was written to last.
    Latest,
    Session(String),
}

impl Resume {
    /// `--resume` with no value arrives as an empty string, which is clap's
    /// way of saying "the flag was there but bare".
    fn parse(flag: Option<&str>) -> Self {
        match flag {
            None => Self::No,
            Some(id) if id.trim().is_empty() => Self::Latest,
            Some(id) => Self::Session(id.to_string()),
        }
    }
}

impl Cli {
    /// The subcommand to execute. Named `resolve_command` rather than
    /// `command` so it does not shadow `clap::CommandFactory::command`.
    pub fn resolve_command(&self) -> Command {
        match &self.command {
            Some(Command::Run { prompt }) => Command::Run {
                prompt: prompt.clone(),
            },
            Some(Command::Chat { resume }) => Command::Chat {
                resume: resume.clone(),
            },
            Some(Command::Sessions { delete }) => Command::Sessions {
                delete: delete.clone(),
            },
            Some(Command::Tools) => Command::Tools,
            Some(Command::Doctor) => Command::Doctor,
            // Bare `agent` drops into the REPL, like `psql` or `irb`.
            None => Command::Chat { resume: None },
        }
    }
}

impl Command {
    /// What `chat` was asked to continue from.
    pub fn resume(&self) -> Resume {
        match self {
            Self::Chat { resume } => Resume::parse(resume.as_deref()),
            _ => Resume::No,
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
        let command = Cli::parse_from(["agent"]).resolve_command();
        assert!(matches!(command, Command::Chat { .. }));
        assert_eq!(command.resume(), Resume::No);
    }

    #[test]
    fn resume_takes_an_optional_session_id() {
        // The bare flag is the common case - "carry on from where I was" -
        // and naming a session is the exception, so the id must be optional
        // rather than a second flag.
        assert_eq!(
            Cli::parse_from(["agent", "chat", "--resume"])
                .resolve_command()
                .resume(),
            Resume::Latest
        );
        assert_eq!(
            Cli::parse_from(["agent", "chat", "--resume", "abc-123"])
                .resolve_command()
                .resume(),
            Resume::Session("abc-123".into())
        );
        assert_eq!(
            Cli::parse_from(["agent", "chat"])
                .resolve_command()
                .resume(),
            Resume::No
        );
    }

    #[test]
    fn sessions_lists_or_deletes() {
        assert!(matches!(
            Cli::parse_from(["agent", "sessions"]).resolve_command(),
            Command::Sessions { delete: None }
        ));
        match Cli::parse_from(["agent", "sessions", "--delete", "abc"]).resolve_command() {
            Command::Sessions { delete } => assert_eq!(delete.as_deref(), Some("abc")),
            other => panic!("expected sessions, got {other:?}"),
        }
    }

    #[test]
    fn global_flags_work_after_the_subcommand() {
        let cli = Cli::parse_from(["agent", "--yes", "-m", "cloud/claude-sonnet-5", "chat"]);
        assert!(cli.yes);
        assert_eq!(cli.model.as_deref(), Some("cloud/claude-sonnet-5"));
    }

    #[test]
    fn prompt_injection_flags_are_parsed() {
        let cli = Cli::parse_from([
            "agent",
            "--system-prompt-file",
            "prompts/agent.md",
            "--append-system-prompt",
            "Reply in Japanese.",
            "run",
            "hi",
        ]);
        assert_eq!(
            cli.system_prompt_file,
            Some(PathBuf::from("prompts/agent.md"))
        );
        assert_eq!(
            cli.append_system_prompt.as_deref(),
            Some("Reply in Japanese.")
        );
    }
}
