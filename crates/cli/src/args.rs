use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};

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

        /// What stdout carries. `text` is the answer as prose; `json` is one
        /// JSON object when the turn ends; `stream-json` is one JSON object
        /// per line, an event each, ending in the same result object. In the
        /// JSON formats stdout holds JSON only - progress stays on stderr.
        #[arg(long, value_enum, value_name = "FORMAT", default_value_t = OutputFormat::Text)]
        output_format: OutputFormat,
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

    /// Run a task suite against the configured model and print a JSON report
    /// on stdout. Exits 0 when every task passed, 3 when every attempt ran
    /// but some did not pass, 1 when some attempt could not run, 130 when
    /// interrupted.
    Eval(EvalArgs),
}

/// `agent eval`'s own flags. The global ones still apply: `--model`,
/// `--max-iterations` (a task's `max_iterations` wins) and the prompt flags.
#[derive(Debug, Clone, Args)]
pub struct EvalArgs {
    /// The suite file (JSON). Fixture and prompt paths in it are relative to
    /// the file's own directory.
    #[arg(value_name = "TASKS")]
    pub tasks: PathBuf,

    /// Run every task this many times.
    #[arg(long, value_name = "N", default_value_t = 1,
          value_parser = clap::value_parser!(u32).range(1..))]
    pub repeat: u32,

    /// Directory for the run's session logs (written to `<DIR>/sessions/`).
    #[arg(long, value_name = "DIR", default_value = ".agent/eval")]
    pub out: PathBuf,

    /// Keep each attempt's scratch workspace instead of deleting it. The
    /// report names where each one is.
    #[arg(long)]
    pub keep: bool,

    /// Start the MCP servers AGENT_MCP_CONFIG lists. Off by default: every
    /// call is approved in eval, and a server acts on whatever it is
    /// connected to, not on the scratch copy.
    #[arg(long)]
    pub allow_mcp: bool,

    /// Offer web_fetch and web_search when AGENT_WEB_FETCH / AGENT_WEB_SEARCH
    /// enable them. Off by default: every call is approved in eval, and a
    /// fetched URL or a search query is a message sent out.
    #[arg(long)]
    pub allow_web: bool,
}

/// What `agent run` writes on stdout.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, ValueEnum)]
pub enum OutputFormat {
    /// The answer as prose, streamed as it arrives.
    #[default]
    Text,
    /// One JSON object, printed when the turn ends.
    Json,
    /// One JSON object per line: every event, then the result.
    StreamJson,
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
            Some(Command::Run {
                prompt,
                output_format,
            }) => Command::Run {
                prompt: prompt.clone(),
                output_format: *output_format,
            },
            Some(Command::Chat { resume }) => Command::Chat {
                resume: resume.clone(),
            },
            Some(Command::Sessions { delete }) => Command::Sessions {
                delete: delete.clone(),
            },
            Some(Command::Tools) => Command::Tools,
            Some(Command::Doctor) => Command::Doctor,
            Some(Command::Eval(args)) => Command::Eval(args.clone()),
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
            Command::Run {
                prompt,
                output_format,
            } => {
                assert_eq!(prompt.join(" "), "list the files");
                assert_eq!(output_format, OutputFormat::Text, "prose stays the default");
            }
            other => panic!("expected run, got {other:?}"),
        }
    }

    #[test]
    fn run_takes_an_output_format() {
        for (flag, expected) in [
            ("text", OutputFormat::Text),
            ("json", OutputFormat::Json),
            ("stream-json", OutputFormat::StreamJson),
        ] {
            match Cli::parse_from(["agent", "run", "--output-format", flag, "hi"]).resolve_command()
            {
                Command::Run { output_format, .. } => assert_eq!(output_format, expected),
                other => panic!("expected run, got {other:?}"),
            }
        }
        assert!(
            Cli::try_parse_from(["agent", "run", "--output-format", "yaml", "hi"]).is_err(),
            "an unknown format must be refused, not fall back to prose"
        );
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
    fn eval_takes_a_suite_and_its_own_flags() {
        match Cli::parse_from(["agent", "eval", "suite.json", "--repeat", "3", "--keep"])
            .resolve_command()
        {
            Command::Eval(args) => {
                assert_eq!(args.tasks, PathBuf::from("suite.json"));
                assert_eq!(args.repeat, 3);
                assert!(args.keep);
                assert_eq!(args.out, PathBuf::from(".agent/eval"));
                assert!(
                    !args.allow_mcp && !args.allow_web,
                    "tools that reach outside the scratch copy are opt-in"
                );
            }
            other => panic!("expected eval, got {other:?}"),
        }
        match Cli::parse_from(["agent", "eval", "suite.json", "--allow-mcp", "--allow-web"])
            .resolve_command()
        {
            Command::Eval(args) => assert!(args.allow_mcp && args.allow_web),
            other => panic!("expected eval, got {other:?}"),
        }
        assert!(
            Cli::try_parse_from(["agent", "eval", "suite.json", "--repeat", "0"]).is_err(),
            "zero repeats would measure nothing"
        );
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
