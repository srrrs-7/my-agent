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
mod notify;
mod render;

use std::io::IsTerminal;
use std::process::ExitCode;
use std::time::Duration;

use anyhow::Result;
use clap::Parser;

use crate::args::{Cli, Command};

/// How long exit waits for blocking work still running on the runtime.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(1);

// The runtime is built by hand instead of with `#[tokio::main]` so that its
// shutdown can be bounded. Dropping a runtime waits for every blocking-pool
// thread, and a DNS lookup (`getaddrinfo`) for a notification webhook runs on
// one and cannot be cancelled. A resolver that hangs would then keep the
// process alive long after `Notifications::drain` gave up on it. Shutting
// down with a timeout abandons such a thread instead, and still returns an
// `ExitCode` normally - unlike `process::exit`, destructors and stdout
// flushing run.
fn main() -> ExitCode {
    // `.env` is a convenience for local runs; real environment variables win.
    let _ = dotenvy::dotenv();

    let cli = Cli::parse();
    agent_infrastructure::telemetry::init("warn");

    // The same configuration `#[tokio::main]` produced.
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("error: cannot start the async runtime: {error}");
            return ExitCode::FAILURE;
        }
    };

    let result = runtime.block_on(dispatch(cli));
    runtime.shutdown_timeout(SHUTDOWN_GRACE);

    match result {
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
    let app = composition::build(&cli, interactive).await?;

    let command = cli.resolve_command();
    let resume = command.resume();

    Ok(match command {
        Command::Run { prompt } => commands::run::execute(&app, prompt.join(" ")).await?,
        Command::Chat { .. } => {
            commands::chat::execute(&app, resume).await?;
            ExitCode::SUCCESS
        }
        Command::Sessions { delete } => {
            match delete {
                Some(id) => commands::sessions::delete(&app, &id).await?,
                None => commands::sessions::list(&app).await?,
            }
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

/// Fresh identifier for a conversation.
///
/// It names the session's log file, so it has to stay unique across runs and
/// not merely within a process - which is what rules out a counter.
pub(crate) fn session_id() -> String {
    uuid::Uuid::new_v4().to_string()
}
