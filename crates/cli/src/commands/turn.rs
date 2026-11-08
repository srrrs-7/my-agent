//! One turn, as both `run` and `chat` drive it.
//!
//! The steps around the loop have to stay in step between the two: the
//! notifier is told a turn began before it runs and how it ended right after,
//! and Ctrl-C is routed through [`drive`] the whole time. Kept in one place so
//! a step added for one subcommand is not missing from the other.

use agent_application::agent::{AgentOutcome, Interrupt, Session};
use agent_application::error::AppError;

use crate::composition::Application;
use crate::ctrl_c::{CtrlC, Sigint, TurnEnd, drive};

/// Runs `prompt` as one turn of `session` and tells the notifier how it ended.
///
/// [`TurnEnd::Abandoned`] when Ctrl-C was pressed again while the turn was
/// stopping: the turn has no result, so the notifier is told nothing, and the
/// caller should exit with the code.
pub(crate) async fn run_turn(
    app: &Application,
    session: &mut Session,
    prompt: impl Into<String>,
    sigint: &mut Sigint,
    keys: &mut CtrlC,
) -> TurnEnd<Result<AgentOutcome, AppError>> {
    app.notifications.begin_turn(&session.id);
    let interrupt = Interrupt::new();
    let turn = app.agent.run_until(session, prompt, &interrupt);
    let end = drive(turn, &interrupt, sigint, keys).await;
    if let TurnEnd::Finished(result) = &end {
        // An interrupted turn is `Ok` too; the notifier's policy is what
        // keeps it quiet.
        app.notifications.turn_finished(result);
    }
    end
}
