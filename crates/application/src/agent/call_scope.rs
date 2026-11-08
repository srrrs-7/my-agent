//! Which call a tool is running for.
//!
//! [`Tool::execute`](agent_domain::ports::tool::Tool::execute) is handed the
//! call's arguments and nothing else - deliberately, so that a tool never has
//! to know about call ids or sessions, and so that the decorators stacked
//! around every tool (timeout, hooks, redaction, ...) have one thing to
//! forward. `spawn_agent` is the one tool that needs more: its sub-agent's
//! session is named after the session and the call that started it (see
//! [`subagent_session_id`](agent_domain::ports::conversation_log::subagent_session_id)),
//! and its sub-agent's events are attributed to that call.
//!
//! So the dispatcher runs every call inside a [`CallScope`], and a tool that
//! wants to know reads [`current`] while it runs. Exact by construction: the
//! scope is the call being polled, so two calls with identical arguments in
//! one turn still get their own ids - which a ledger keyed by
//! `(tool, arguments)`, the way the hooks find a call id, could not promise -
//! and nothing is left behind by a call that was approved but never run.
//!
//! The scope is set for the duration of each poll of the call's future and
//! put back afterwards, the way a task-local works, and with `std` alone so
//! this crate stays without a runtime. The one thing it asks of the code in
//! between is that the tool's future is polled where the dispatcher polls
//! it: a decorator that moved `execute` onto another task would run it
//! outside the scope. Every decorator in the tree awaits its inner tool in
//! place; `spawn_agent` refuses to run without a scope rather than guess, so
//! one that stopped doing so would fail loudly - and
//! `crates/cli/tests/composition.rs` runs the real stack to catch it.

use std::cell::RefCell;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use agent_domain::model::tool::ToolCallId;

/// The call a tool is running for, and the session it belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallScope {
    pub session_id: String,
    pub call_id: ToolCallId,
}

thread_local! {
    static CURRENT: RefCell<Option<Arc<CallScope>>> = const { RefCell::new(None) };
}

/// The call the code now running is part of, if a dispatcher started it.
pub fn current() -> Option<Arc<CallScope>> {
    CURRENT.with(|current| current.borrow().clone())
}

/// `work`, with [`current`] answering `scope` while it is polled.
pub(crate) fn within<F: Future>(scope: CallScope, work: F) -> Scoped<F> {
    Scoped {
        scope: Arc::new(scope),
        work: Box::pin(work),
    }
}

/// Future returned by [`within`].
#[must_use = "futures do nothing unless polled"]
pub(crate) struct Scoped<F> {
    scope: Arc<CallScope>,
    /// Boxed so `Scoped` is `Unpin` and needs no pin projection.
    work: Pin<Box<F>>,
}

impl<F: Future> Future for Scoped<F> {
    type Output = F::Output;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<F::Output> {
        let this = &mut *self;
        let outer = CURRENT.with(|current| current.replace(Some(this.scope.clone())));
        // Put back on drop rather than after the poll, so a panicking tool
        // does not leave its scope behind for whatever this thread polls
        // next. The outer scope matters too: a sub-agent's calls run inside
        // the scope of the call that started it.
        let _restore = Restore(Some(outer));
        this.work.as_mut().poll(cx)
    }
}

struct Restore(Option<Option<Arc<CallScope>>>);

impl Drop for Restore {
    fn drop(&mut self) {
        if let Some(outer) = self.0.take() {
            CURRENT.with(|current| *current.borrow_mut() = outer);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::FutureExt as _;

    fn scope(call: &str) -> CallScope {
        CallScope {
            session_id: "s".into(),
            call_id: ToolCallId::new(call),
        }
    }

    fn seen() -> Option<String> {
        current().map(|scope| scope.call_id.to_string())
    }

    #[test]
    fn nothing_is_in_scope_outside_a_call() {
        assert_eq!(seen(), None);
    }

    #[test]
    fn a_call_sees_its_own_scope_and_leaves_none_behind() {
        let inside = within(scope("c1"), async { seen() }).now_or_never();
        assert_eq!(inside, Some(Some("c1".to_string())));
        assert_eq!(seen(), None);
    }

    #[test]
    fn a_nested_call_sees_its_own_and_hands_the_outer_one_back() {
        let observed = within(scope("outer"), async {
            let before = seen();
            let inner = within(scope("inner"), async { seen() }).await;
            (before, inner, seen())
        })
        .now_or_never()
        .unwrap();
        assert_eq!(
            observed,
            (
                Some("outer".to_string()),
                Some("inner".to_string()),
                Some("outer".to_string())
            )
        );
    }

    /// Pending once, waking itself so the executor polls again.
    async fn yield_once() {
        let mut yielded = false;
        futures::future::poll_fn(|cx| {
            if yielded {
                return std::task::Poll::Ready(());
            }
            yielded = true;
            cx.waker().wake_by_ref();
            std::task::Poll::Pending
        })
        .await;
    }

    #[test]
    fn calls_polled_side_by_side_each_see_their_own() {
        let (first, second) = futures::executor::block_on(futures::future::join(
            within(scope("a"), async {
                yield_once().await;
                seen()
            }),
            within(scope("b"), async {
                yield_once().await;
                seen()
            }),
        ));
        assert_eq!(first.as_deref(), Some("a"));
        assert_eq!(second.as_deref(), Some("b"));
    }
}
