//! `agent run <prompt>` - one turn, then exit.

use std::process::ExitCode;

use agent_application::agent::Session;
use agent_domain::ports::events::FinishReason;
use anyhow::Result;

use crate::commands::turn::run_turn;
use crate::composition::Application;
use crate::ctrl_c::{CtrlC, EXIT_INTERRUPTED, Sigint, TurnEnd};
use crate::notify::DRAIN_TIMEOUT;
use crate::session_id;

/// Returned when the loop ran out of iterations before producing an answer, so
/// scripts can tell "answered" from "gave up".
const EXIT_INCOMPLETE: u8 = 2;

pub async fn execute(app: &Application, prompt: String) -> Result<ExitCode> {
    let mut sigint = Sigint::listen()?;
    let mut keys = CtrlC::new();
    let mut session = Session::new(session_id());
    let result = match run_turn(app, &mut session, prompt, &mut sigint, &mut keys).await {
        TurnEnd::Finished(result) => result,
        TurnEnd::Abandoned(code) => {
            // Notifications already in flight still get their bounded wait.
            app.notifications.drain(DRAIN_TIMEOUT).await;
            return Ok(code);
        }
    };

    // The notifier was told before the result is acted on; wait for the
    // delivery: the process is about to exit, and a spawned send dies with
    // it. Neither step can change what is returned - the exit code is the
    // turn's alone.
    app.notifications.drain(DRAIN_TIMEOUT).await;
    let outcome = result?;

    Ok(match outcome.reason {
        FinishReason::Completed => ExitCode::SUCCESS,
        FinishReason::Interrupted => ExitCode::from(EXIT_INTERRUPTED),
        FinishReason::MaxIterations { .. } | FinishReason::Stopped { .. } => {
            ExitCode::from(EXIT_INCOMPLETE)
        }
    })
}
