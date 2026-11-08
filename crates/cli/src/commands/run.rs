//! `agent run <prompt>` - one turn, then exit.

use std::process::ExitCode;

use agent_application::agent::{Interrupt, Session};
use agent_domain::ports::events::FinishReason;
use anyhow::Result;

use crate::composition::Application;
use crate::ctrl_c::{CtrlC, EXIT_INTERRUPTED, Sigint, TurnEnd, drive};
use crate::notify::DRAIN_TIMEOUT;
use crate::session_id;

/// Returned when the loop ran out of iterations before producing an answer, so
/// scripts can tell "answered" from "gave up".
const EXIT_INCOMPLETE: u8 = 2;

pub async fn execute(app: &Application, prompt: String) -> Result<ExitCode> {
    let mut sigint = Sigint::listen()?;
    let mut keys = CtrlC::new();
    let mut session = Session::new(session_id());
    app.notifications.begin_turn(&session.id);
    let interrupt = Interrupt::new();
    let turn = app.agent.run_until(&mut session, prompt, &interrupt);
    let result = match drive(turn, &interrupt, &mut sigint, &mut keys).await {
        TurnEnd::Finished(result) => result,
        TurnEnd::Abandoned(code) => {
            // Notifications already in flight still get their bounded wait.
            app.notifications.drain(DRAIN_TIMEOUT).await;
            return Ok(code);
        }
    };

    // Notify on the result before acting on it, and wait for the delivery:
    // the process is about to exit, and a spawned send dies with it. Neither
    // step can change what is returned - the exit code is the turn's alone.
    app.notifications.turn_finished(&result);
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
