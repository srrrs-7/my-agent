//! `agent` - a sandboxed, loop-driven coding agent.
//!
//! Layer map (dependencies point inwards only):
//!
//! ```text
//!   agent-cli ............ this crate: argv, rendering, composition root
//!     -> agent-application  the loop, the tools, prompt assembly
//!     -> agent-infrastructure  HTTP clients, filesystem, config
//!          -> agent-domain     entities, value objects, ports
//! ```

mod approval;
mod args;
mod commands;
mod composition;
mod input;
mod render;

use std::io::IsTerminal;
use std::process::ExitCode;

use anyhow::Result;
use clap::Parser;

use crate::args::{Cli, Command};

#[tokio::main]
async fn main() -> ExitCode {
    // `.env` is a convenience for local runs; real environment variables win.
    let _ = dotenvy::dotenv();

    let cli = Cli::parse();
    agent_infrastructure::telemetry::init("warn");

    match dispatch(cli).await {
        Ok(code) => code,
        Err(error) => {
            // `{:#}` prints the whole context chain on one line, which is what
            // a CLI user wants; the backtrace stays behind RUST_BACKTRACE.
            eprintln!("error: {error:#}");
            ExitCode::FAILURE
        }
    }
}

async fn dispatch(cli: Cli) -> Result<ExitCode> {
    // Without a TTY there is nobody to answer an approval prompt, so the gate
    // has to deny rather than block forever.
    let interactive = std::io::stdin().is_terminal();
    let app = composition::build(&cli, interactive)?;

    Ok(match cli.resolve_command() {
        Command::Run { prompt } => commands::run::execute(&app, prompt.join(" ")).await?,
        Command::Chat => {
            commands::chat::execute(&app).await?;
            ExitCode::SUCCESS
        }
        Command::Tools => {
            commands::tools::execute(&app);
            ExitCode::SUCCESS
        }
        Command::Doctor => {
            commands::doctor::execute(&app).await?;
            ExitCode::SUCCESS
        }
    })
}

/// Fresh identifier for a conversation. Sessions are not persisted yet, so this
/// only has to be unique within a process.
pub(crate) fn session_id() -> String {
    uuid::Uuid::new_v4().to_string()
}
