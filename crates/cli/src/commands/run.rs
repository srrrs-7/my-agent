//! `agent run <prompt>` - one turn, then exit.
//!
//! The exit code is the same in every output format: 0 answered, 1 failed,
//! 2 gave up (iterations or the provider's own stop), 130 interrupted. The
//! JSON formats print it in their result too.

use std::process::ExitCode;
use std::sync::Arc;

use agent_application::agent::{AgentOutcome, Session};
use agent_application::error::AppError;
use agent_domain::ports::events::FinishReason;
use anyhow::Result;

use crate::args::Cli;
use crate::commands::turn::run_turn;
use crate::composition::{self, Application};
use crate::ctrl_c::{CtrlC, EXIT_INTERRUPTED, Sigint, TurnEnd};
use crate::notify::DRAIN_TIMEOUT;
use crate::output::{Ending, JsonLinesSink};
use crate::session_id;

/// Returned when the loop ran out of iterations before producing an answer, so
/// scripts can tell "answered" from "gave up".
const EXIT_INCOMPLETE: u8 = 2;

/// What `main` returns for an `Err`.
const EXIT_FAILURE: u8 = 1;

pub async fn execute(app: &Application, prompt: String) -> Result<ExitCode> {
    let (_, end) = turn(app, prompt).await?;
    match end {
        TurnEnd::Finished(result) => Ok(ExitCode::from(exit_code(&result?.reason))),
        TurnEnd::Abandoned(code) => Ok(code),
    }
}

/// `agent run --output-format json|stream-json`.
///
/// Builds the agent itself, narrated to a [`JsonLinesSink`] rather than the
/// terminal, so that stdout carries JSON from the first byte. Every way out -
/// startup failure included - prints a result object first, so a script that
/// parses stdout always has something to parse; the error still reaches
/// stderr and the exit code through `main`, as in `text`.
pub async fn execute_structured(
    cli: &Cli,
    interactive: bool,
    output: JsonLinesSink,
    prompt: String,
) -> Result<ExitCode> {
    let output = Arc::new(output);

    let app = match composition::build_with_events(cli, interactive, output.clone()).await {
        Ok(app) => app,
        Err(error) => {
            output.finish(Ending::Failed(format!("{error:#}")), None, EXIT_FAILURE);
            return Err(error);
        }
    };
    let (session_id, end) = match turn(&app, prompt).await {
        Ok(turn) => turn,
        Err(error) => {
            output.finish(Ending::Failed(format!("{error:#}")), None, EXIT_FAILURE);
            return Err(error);
        }
    };

    match end {
        TurnEnd::Finished(Ok(outcome)) => {
            let code = exit_code(&outcome.reason);
            output.finish(Ending::Finished(&outcome), Some(&session_id), code);
            Ok(ExitCode::from(code))
        }
        TurnEnd::Finished(Err(error)) => {
            let error = anyhow::Error::from(error);
            output.finish(
                Ending::Failed(format!("{error:#}")),
                Some(&session_id),
                EXIT_FAILURE,
            );
            Err(error)
        }
        TurnEnd::Abandoned(code) => {
            output.finish(Ending::Abandoned, Some(&session_id), EXIT_INTERRUPTED);
            Ok(code)
        }
    }
}

/// One turn in a fresh session, with the notifier's deliveries waited for.
///
/// The notifier was told before the result is acted on; wait for the delivery:
/// the process is about to exit, and a spawned send dies with it. Neither step
/// can change what is returned - the exit code is the turn's alone. An
/// abandoned turn's notifications already in flight get the same bounded
/// wait.
async fn turn(
    app: &Application,
    prompt: String,
) -> Result<(String, TurnEnd<Result<AgentOutcome, AppError>>)> {
    let mut sigint = Sigint::listen()?;
    let mut keys = CtrlC::new();
    let mut session = Session::new(session_id());
    let end = run_turn(app, &mut session, prompt, &mut sigint, &mut keys).await;
    app.notifications.drain(DRAIN_TIMEOUT).await;
    Ok((session.id, end))
}

fn exit_code(reason: &FinishReason) -> u8 {
    match reason {
        FinishReason::Completed => 0,
        FinishReason::Interrupted => EXIT_INTERRUPTED,
        FinishReason::MaxIterations { .. } | FinishReason::Stopped { .. } => EXIT_INCOMPLETE,
    }
}
