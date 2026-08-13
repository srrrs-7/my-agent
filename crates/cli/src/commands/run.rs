//! `agent run <prompt>` - one turn, then exit.

use std::process::ExitCode;

use agent_application::agent::Session;
use anyhow::Result;

use crate::composition::Application;
use crate::session_id;

/// Returned when the loop ran out of iterations before producing an answer, so
/// scripts can tell "answered" from "gave up".
const EXIT_INCOMPLETE: u8 = 2;

pub async fn execute(app: &Application, prompt: String) -> Result<ExitCode> {
    let mut session = Session::new(session_id());
    let outcome = app.agent.run(&mut session, prompt).await?;

    Ok(if outcome.is_complete() {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(EXIT_INCOMPLETE)
    })
}
