//! What Ctrl-C means, and listening for it.
//!
//! The meaning depends on where the session is, not on how quickly the key
//! was pressed again - a time window would make the second press depend on
//! the machine's speed, and could not be tested without sleeping:
//!
//! | where                              | Ctrl-C                         |
//! |------------------------------------|--------------------------------|
//! | at the chat prompt                 | quit (0, the same as Ctrl-D)   |
//! | a turn is running                  | ask the turn to stop           |
//! | the turn was asked and is stopping | give up on it and quit (130)   |
//!
//! Asking is cooperative ([`Interrupt`]): the turn stops at a point where it
//! can still answer every tool call it made, so the conversation survives.
//! Only the third row drops the turn's future, and only because the process
//! is about to exit.
//!
//! The handler is installed by `chat`, `run` and `eval` alone (`eval` treats
//! each attempt as a turn). Once installed, SIGINT
//! no longer kills the process for the rest of its life, so the subcommands
//! that do not route it anywhere (`doctor`, `sessions`, ...) keep the
//! default.

use std::future::Future;
use std::process::ExitCode;

use agent_application::agent::Interrupt;
use anyhow::{Context as _, Result};

/// The shell's convention for "ended by SIGINT" (128 + 2).
pub const EXIT_INTERRUPTED: u8 = 130;

/// What one press of Ctrl-C should do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reaction {
    /// Leave quietly: nothing was running.
    Quit,
    /// Ask the running turn to stop.
    Interrupt,
    /// The turn was already asked; stop waiting for it and exit.
    Abandon,
}

impl Reaction {
    /// The exit code, for the reactions that end the process.
    pub fn exit_code(self) -> Option<ExitCode> {
        match self {
            Self::Quit => Some(ExitCode::SUCCESS),
            Self::Interrupt => None,
            Self::Abandon => Some(ExitCode::from(EXIT_INTERRUPTED)),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Phase {
    #[default]
    Idle,
    Running,
    Stopping,
}

/// The state machine in the table above.
#[derive(Debug, Default)]
pub struct CtrlC {
    phase: Phase,
}

impl CtrlC {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn turn_started(&mut self) {
        self.phase = Phase::Running;
    }

    pub fn turn_ended(&mut self) {
        self.phase = Phase::Idle;
    }

    pub fn pressed(&mut self) -> Reaction {
        match self.phase {
            Phase::Idle => Reaction::Quit,
            Phase::Running => {
                self.phase = Phase::Stopping;
                Reaction::Interrupt
            }
            Phase::Stopping => Reaction::Abandon,
        }
    }
}

/// SIGINT, as a stream of presses.
///
/// One long-lived listener rather than a fresh `tokio::signal::ctrl_c()` per
/// wait: a press that lands between two waits is held for the next one
/// instead of being lost.
pub struct Sigint {
    source: Source,
}

enum Source {
    #[cfg(unix)]
    Os(tokio::signal::unix::Signal),
    #[cfg(not(unix))]
    Os,
    #[cfg(test)]
    Fake(tokio::sync::mpsc::UnboundedReceiver<()>),
}

impl Sigint {
    /// Installs the handler. From here on Ctrl-C no longer ends the process
    /// by itself.
    pub fn listen() -> Result<Self> {
        #[cfg(unix)]
        let source = Source::Os(
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
                .context("cannot listen for Ctrl-C")?,
        );
        #[cfg(not(unix))]
        let source = Source::Os;
        Ok(Self { source })
    }

    /// Resolves on the next press. Cancel-safe.
    pub async fn recv(&mut self) {
        match &mut self.source {
            #[cfg(unix)]
            Source::Os(signal) => {
                if signal.recv().await.is_none() {
                    // The driver is gone; no press will ever arrive.
                    std::future::pending::<()>().await;
                }
            }
            #[cfg(not(unix))]
            Source::Os => {
                if tokio::signal::ctrl_c().await.is_err() {
                    std::future::pending::<()>().await;
                }
            }
            #[cfg(test)]
            Source::Fake(presses) => {
                if presses.recv().await.is_none() {
                    std::future::pending::<()>().await;
                }
            }
        }
    }

    /// Presses that come from the test instead of the keyboard.
    #[cfg(test)]
    pub(crate) fn fake() -> (tokio::sync::mpsc::UnboundedSender<()>, Self) {
        let (press, presses) = tokio::sync::mpsc::unbounded_channel();
        (
            press,
            Self {
                source: Source::Fake(presses),
            },
        )
    }
}

/// How a driven turn ended.
#[derive(Debug, PartialEq)]
pub enum TurnEnd<T> {
    /// The turn returned - normally, with an error, or interrupted.
    Finished(T),
    /// Ctrl-C was pressed again while the turn was stopping. Its future has
    /// been dropped and the process should exit with `code`.
    Abandoned(ExitCode),
}

/// Runs `turn`, routing Ctrl-C presses through `keys` while it does.
///
/// `interrupt` must be the one `turn` was given.
pub async fn drive<F: Future>(
    turn: F,
    interrupt: &Interrupt,
    sigint: &mut Sigint,
    keys: &mut CtrlC,
) -> TurnEnd<F::Output> {
    keys.turn_started();
    let mut turn = std::pin::pin!(turn);
    let end = loop {
        tokio::select! {
            // A press is taken before a result that is ready at the same
            // moment. Otherwise it would stay queued, and the prompt that
            // follows would read it as "quit" - ending the session for
            // someone who only meant to stop the turn.
            biased;
            () = sigint.recv() => match keys.pressed() {
                Reaction::Interrupt => {
                    interrupt.fire();
                    eprintln!("\n! interrupting - press Ctrl-C again to quit");
                }
                reaction => {
                    // `Quit` cannot happen while a turn runs; if it somehow
                    // did, leaving is still the safe reading of the key.
                    let code = reaction
                        .exit_code()
                        .unwrap_or(ExitCode::from(EXIT_INTERRUPTED));
                    break TurnEnd::Abandoned(code);
                }
            },
            output = &mut turn => break TurnEnd::Finished(output),
        }
    };
    keys.turn_ended();
    end
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn at_the_prompt_ctrl_c_quits_cleanly() {
        let mut keys = CtrlC::new();
        let reaction = keys.pressed();
        assert_eq!(reaction, Reaction::Quit);
        assert_eq!(reaction.exit_code(), Some(ExitCode::SUCCESS));
    }

    #[test]
    fn during_a_turn_the_first_press_interrupts_and_the_second_abandons() {
        let mut keys = CtrlC::new();
        keys.turn_started();
        assert_eq!(keys.pressed(), Reaction::Interrupt);
        assert_eq!(
            Reaction::Interrupt.exit_code(),
            None,
            "the process lives on"
        );
        assert_eq!(keys.pressed(), Reaction::Abandon);
        assert_eq!(
            Reaction::Abandon.exit_code(),
            Some(ExitCode::from(EXIT_INTERRUPTED))
        );
    }

    #[test]
    fn a_finished_turn_puts_the_next_press_back_at_the_prompt() {
        let mut keys = CtrlC::new();
        keys.turn_started();
        assert_eq!(keys.pressed(), Reaction::Interrupt);
        keys.turn_ended();
        assert_eq!(keys.pressed(), Reaction::Quit);

        // And a new turn starts over, rather than remembering the old press.
        keys.turn_started();
        assert_eq!(keys.pressed(), Reaction::Interrupt);
    }

    /// A turn that stops as soon as it is asked to, like the real loop.
    async fn cooperative(interrupt: &Interrupt) -> &'static str {
        interrupt.fired().await;
        "stopped"
    }

    #[tokio::test]
    async fn one_press_asks_the_turn_to_stop_and_waits_for_it() {
        let (press, mut sigint) = Sigint::fake();
        let mut keys = CtrlC::new();
        let interrupt = Interrupt::new();
        press.send(()).unwrap();

        let end = drive(cooperative(&interrupt), &interrupt, &mut sigint, &mut keys).await;

        assert_eq!(end, TurnEnd::Finished("stopped"));
        assert!(interrupt.is_fired());
        assert_eq!(keys.pressed(), Reaction::Quit, "back at the prompt");
    }

    #[tokio::test]
    async fn a_second_press_gives_up_on_a_turn_that_does_not_stop() {
        let (press, mut sigint) = Sigint::fake();
        let mut keys = CtrlC::new();
        let interrupt = Interrupt::new();
        press.send(()).unwrap();
        press.send(()).unwrap();

        let end = drive(
            std::future::pending::<()>(),
            &interrupt,
            &mut sigint,
            &mut keys,
        )
        .await;

        assert_eq!(end, TurnEnd::Abandoned(ExitCode::from(EXIT_INTERRUPTED)));
        assert!(interrupt.is_fired(), "the first press still asked politely");
    }

    #[tokio::test]
    async fn a_press_racing_a_finished_turn_is_not_left_for_the_prompt() {
        let (press, mut sigint) = Sigint::fake();
        let mut keys = CtrlC::new();
        let interrupt = Interrupt::new();
        press.send(()).unwrap();

        let end = drive(async { "done" }, &interrupt, &mut sigint, &mut keys).await;

        assert_eq!(end, TurnEnd::Finished("done"));
        // Nothing is queued: the prompt would block here rather than quit.
        tokio::select! {
            biased;
            () = sigint.recv() => panic!("the press leaked out of the turn"),
            () = std::future::ready(()) => {}
        }
    }
}
