//! `agent run <prompt>` - one turn, then exit.

use std::process::ExitCode;

use agent_application::agent::Session;
use anyhow::Result;

use crate::composition::Application;
use crate::notify::DRAIN_TIMEOUT;
use crate::session_id;

/// Returned when the loop ran out of iterations before producing an answer, so
/// scripts can tell "answered" from "gave up".
const EXIT_INCOMPLETE: u8 = 2;

pub async fn execute(app: &Application, prompt: String) -> Result<ExitCode> {
    let mut session = Session::new(session_id());
    app.notifications.begin_turn(&session.id);
    let result = app.agent.run(&mut session, prompt).await;

    // Notify on the result before acting on it, and wait for the delivery:
    // the process is about to exit, and a spawned send dies with it. Neither
    // step can change what is returned - the exit code is the turn's alone.
    app.notifications.turn_finished(&result);
    app.notifications.drain(DRAIN_TIMEOUT).await;
    let outcome = result?;

    Ok(if outcome.is_complete() {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(EXIT_INCOMPLETE)
    })
}
