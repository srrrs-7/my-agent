//! The `agent` binary: process setup over the `agent_cli` library.
//!
//! Only what belongs to a process lives here - `.env`, logging, the runtime
//! and its bounded shutdown, the exit code. Everything a test may want to
//! build is in the library (see `lib.rs`).

use std::io::IsTerminal;
use std::process::ExitCode;
use std::time::Duration;

use anyhow::Result;
use clap::Parser;

use agent_cli::args::{Cli, Command};
use agent_cli::{commands, composition, dotenv};

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
    // First, while the process has one thread - see `load_dotenv`.
    load_dotenv();

    let cli = Cli::parse();
    // `-v` also shows which provider served each request and why - summaries
    // included, which emit no event of their own.
    agent_infrastructure::telemetry::init(if cli.verbose {
        "warn,agent::route=info"
    } else {
        "warn"
    });

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

/// `.env` is a convenience for local runs: it fills in what the real
/// environment leaves unset, except the settings `agent_cli::dotenv` keeps
/// for the real environment alone. A missing or malformed file is not an
/// error - the entries before a malformed line still apply, as they always
/// did.
fn load_dotenv() {
    let Ok(entries) = dotenvy::dotenv_iter() else {
        return;
    };
    let entries = entries.map_while(Result::ok);
    let applied = dotenv::apply(entries, |key| std::env::var_os(key).is_some());
    for (key, value) in applied.set {
        // SAFETY: called first thing in `main`, before the runtime or any
        // other thread exists, so nothing can be reading the environment
        // concurrently - the condition `set_var` requires. `dotenvy::dotenv`
        // made the same call from the same place.
        unsafe { std::env::set_var(key, value) };
    }
    if let Some(warning) = dotenv::warning(&applied.ignored) {
        eprintln!("{warning}");
    }
}

async fn dispatch(cli: Cli) -> Result<ExitCode> {
    // eval builds one agent per attempt, each over a scratch copy of a
    // fixture. The agent for the real workspace is never built, so nothing -
    // not even session retention - touches it.
    if let Some(Command::Eval(args)) = &cli.command {
        return commands::eval::execute(&cli, args).await;
    }

    // Without a TTY there is nobody to answer an approval prompt, so the gate
    // has to deny rather than block forever.
    let interactive = std::io::stdin().is_terminal();
    let app = composition::build(&cli, interactive).await?;

    let command = cli.resolve_command();
    let resume = command.resume();

    Ok(match command {
        Command::Run { prompt } => commands::run::execute(&app, prompt.join(" ")).await?,
        Command::Chat { .. } => commands::chat::execute(&app, resume).await?,
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
        // Answered above, before an agent was built for the real workspace;
        // repeating it here is harmless, which a panic would not be.
        Command::Eval(args) => commands::eval::execute(&cli, &args).await?,
    })
}
