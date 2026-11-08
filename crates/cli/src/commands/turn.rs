//! One turn, as both `run` and `chat` drive it.
//!
//! The steps around the loop have to stay in step between the two: the
//! notifier and the hooks are told a turn began before it runs and how it
//! ended right after, and Ctrl-C is routed through [`drive`] the whole time.
//! Kept in one place so a step added for one subcommand is not missing from
//! the other.

use agent_application::agent::{AgentOutcome, Interrupt, Session, UserInput};
use agent_application::error::AppError;
use agent_domain::ports::events::FinishReason;

use crate::composition::Application;
use crate::ctrl_c::{CtrlC, Sigint, TurnEnd, drive};

/// Runs `prompt` as one turn of `session` and tells the notifier and the stop
/// hooks how it ended.
///
/// [`TurnEnd::Abandoned`] when Ctrl-C was pressed again while the turn was
/// stopping: the turn has no result, so the notifier is told nothing, and the
/// caller should exit with the code.
pub(crate) async fn run_turn(
    app: &Application,
    session: &mut Session,
    prompt: impl Into<UserInput>,
    sigint: &mut Sigint,
    keys: &mut CtrlC,
) -> TurnEnd<Result<AgentOutcome, AppError>> {
    app.notifications.begin_turn(&session.id);
    if let Some(hooks) = &app.hooks {
        hooks.begin_turn(&session.id);
    }
    let interrupt = Interrupt::new();
    let turn = app.agent.run_until(session, prompt, &interrupt);
    let end = drive(turn, &interrupt, sigint, keys).await;
    if let TurnEnd::Finished(result) = &end {
        // An interrupted turn is `Ok` too; the notifier's policy is what
        // keeps it quiet.
        app.notifications.turn_finished(result);
        // Awaited, not spawned: a stop hook is the operator's "the turn is
        // over" and must not still be running into the next one. Each hook
        // is bounded by its own timeout.
        if let Some(hooks) = &app.hooks {
            hooks.stop(&session.id, stop_reason(result)).await;
        }
    }
    end
}

/// How a turn ended, as the stop hooks are told it.
pub(crate) fn stop_reason(result: &Result<AgentOutcome, AppError>) -> &'static str {
    match result {
        Ok(outcome) => match outcome.reason {
            FinishReason::Completed => "completed",
            FinishReason::MaxIterations { .. } => "max_iterations",
            FinishReason::Stopped { .. } => "stopped",
            FinishReason::Interrupted => "interrupted",
        },
        Err(_) => "error",
    }
}
