//! Cooperative interruption of a running turn.
//!
//! A turn is not stopped by dropping its future. Dropping would leave the
//! conversation wherever the last `.await` happened to be - typically just
//! after an assistant `tool_call` was appended and recorded, with no result to
//! pair it with - and the next request would be rejected by the provider
//! (invariant §4). Instead the caller *asks* the turn to stop, and the turn
//! stops at a point where it can still leave a well-formed history behind:
//! the top of an iteration, a model request, a compaction, or inside the
//! dispatcher, which answers every call it was handed even when it stops.
//!
//! The type is concrete and `std`-only rather than a port. There is nothing to
//! swap: what varies is who *fires* it (a Ctrl-C handler, a test, a future
//! remote control), and that is the caller's business. Keeping it free of any
//! runtime is what lets this crate stay without `tokio`.

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Context, Poll, Waker};

/// A one-way switch shared between whoever wants a turn stopped and the turn.
///
/// Clones share the same state. Once fired it stays fired, which is why a
/// fresh one is made for every turn.
#[derive(Clone, Default)]
pub struct Interrupt {
    inner: Arc<Inner>,
}

#[derive(Default)]
struct Inner {
    fired: AtomicBool,
    /// Every task currently waiting in [`Interrupt::fired`]. Several can wait
    /// at once: the dispatcher races each concurrent read-only call against
    /// the same interrupt.
    wakers: Mutex<Vec<Waker>>,
}

impl Interrupt {
    pub fn new() -> Self {
        Self::default()
    }

    /// Asks the turn to stop. Idempotent.
    pub fn fire(&self) {
        if self.inner.fired.swap(true, Ordering::SeqCst) {
            return;
        }
        let wakers = std::mem::take(&mut *self.wakers());
        for waker in wakers {
            waker.wake();
        }
    }

    pub fn is_fired(&self) -> bool {
        self.inner.fired.load(Ordering::SeqCst)
    }

    /// Resolves once [`Self::fire`] has been called - immediately if it
    /// already was.
    pub fn fired(&self) -> Fired<'_> {
        Fired { interrupt: self }
    }

    /// Runs `work` unless the interrupt fires first, in which case `work` is
    /// dropped where it stands and `None` comes back.
    ///
    /// Only for work that is safe to abandon mid-way: a model request, a
    /// summary that mutates nothing until it has arrived, a tool whose
    /// partial effect the caller will own up to.
    pub async fn guard<F: Future>(&self, work: F) -> Option<F::Output> {
        if self.is_fired() {
            return None;
        }
        let work = std::pin::pin!(work);
        match futures::future::select(work, self.fired()).await {
            futures::future::Either::Left((output, _)) => Some(output),
            futures::future::Either::Right(_) => None,
        }
    }

    fn wakers(&self) -> std::sync::MutexGuard<'_, Vec<Waker>> {
        // A poisoned lock means a waker panicked mid-wake; the list itself is
        // still a valid list, and a stuck interrupt is worse than a stale entry.
        self.inner
            .wakers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }
}

impl std::fmt::Debug for Interrupt {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Interrupt")
            .field("fired", &self.is_fired())
            .finish()
    }
}

/// Future returned by [`Interrupt::fired`].
#[must_use = "futures do nothing unless polled"]
pub struct Fired<'a> {
    interrupt: &'a Interrupt,
}

impl Future for Fired<'_> {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        if self.interrupt.is_fired() {
            return Poll::Ready(());
        }
        let mut wakers = self.interrupt.wakers();
        // Checked again under the lock: `fire` sets the flag *before* taking
        // the lock, so either it sees the waker pushed below or this sees the
        // flag. Without the second check a fire landing between the first
        // check and the push would wake nobody.
        if self.interrupt.is_fired() {
            return Poll::Ready(());
        }
        // The same task polls repeatedly; keep one entry per task, not one per
        // poll.
        if !wakers.iter().any(|waker| waker.will_wake(cx.waker())) {
            wakers.push(cx.waker().clone());
        }
        Poll::Pending
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::FutureExt as _;

    #[test]
    fn starts_unfired_and_stays_fired() {
        let interrupt = Interrupt::new();
        assert!(!interrupt.is_fired());
        interrupt.fire();
        interrupt.fire();
        assert!(interrupt.is_fired());
    }

    #[test]
    fn clones_share_the_switch() {
        let interrupt = Interrupt::new();
        let handle = interrupt.clone();
        handle.fire();
        assert!(interrupt.is_fired());
    }

    #[test]
    fn every_waiter_is_woken() {
        let interrupt = Interrupt::new();
        let mut first = Box::pin(interrupt.fired());
        let mut second = Box::pin(interrupt.fired());
        assert!(first.as_mut().now_or_never().is_none());
        assert!(second.as_mut().now_or_never().is_none());

        interrupt.fire();

        assert!(first.now_or_never().is_some());
        assert!(second.now_or_never().is_some());
    }

    #[test]
    fn guarded_work_is_skipped_once_fired() {
        let interrupt = Interrupt::new();
        interrupt.fire();
        let ran = std::cell::Cell::new(false);
        let outcome = interrupt
            .guard(async {
                ran.set(true);
            })
            .now_or_never();
        assert_eq!(outcome, Some(None));
        assert!(!ran.get(), "fired before the work started, so it never ran");
    }

    #[test]
    fn guarded_work_that_finishes_returns_its_output() {
        let interrupt = Interrupt::new();
        assert_eq!(interrupt.guard(async { 7 }).now_or_never(), Some(Some(7)));
    }

    #[test]
    fn pending_work_gives_way_when_fired() {
        let interrupt = Interrupt::new();
        let mut guarded = Box::pin(interrupt.guard(futures::future::pending::<()>()));
        assert!(guarded.as_mut().now_or_never().is_none());
        interrupt.fire();
        assert_eq!(guarded.now_or_never(), Some(None));
    }
}
