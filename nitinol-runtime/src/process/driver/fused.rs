use crate::error::HandlerError;
use crate::process::driver::{Driver, PipeHandle};
use crate::process::{Process, ProcessContext};

/// Wraps a [`Driver`] so that exhaustion (`next()` returning `None`) silently
/// transitions to a forever-pending state rather than propagating `None` to
/// the lifecycle loop.
///
/// The lifecycle loop interprets `None` from the combined driver tree as
/// `TerminatedReason::Stopped`.  When a user-supplied driver is installed via
/// [`Props::with_driver`](crate::process::Props::with_driver) and subsequently
/// exhausted, `FusedDriver` absorbs the `None` so the Core trio
/// (`MessageDriver` + `PipeDriver` + `StashDriver`) keeps the process alive.
///
/// This preserves the pre-#57 `DynDriverSet` contract: exhausting a
/// user-supplied driver is a non-fatal event. The process continues to handle
/// messages, pipe events, and stash operations until an explicit `Stop` or
/// `Poison` signal arrives.
pub(crate) struct FusedDriver<D> {
    inner: D,
    done: bool,
}

impl<D> FusedDriver<D> {
    pub(crate) fn new(inner: D) -> Self {
        Self { inner, done: false }
    }
}

impl<P, D> Driver<P> for FusedDriver<D>
where
    P: Process,
    D: Driver<P>,
{
    type Event = D::Event;

    async fn next(&mut self) -> Option<Self::Event> {
        if self.done {
            // Inner driver already exhausted: stay pending forever so the
            // sibling Core trio branch in `Combine` can keep firing.
            return std::future::pending::<Option<Self::Event>>().await;
        }
        let ev = self.inner.next().await;
        if ev.is_none() {
            self.done = true;
            return std::future::pending::<Option<Self::Event>>().await;
        }
        ev
    }

    async fn apply(
        &mut self,
        state: &mut P,
        ctx: &mut ProcessContext<P>,
        ev: Self::Event,
    ) -> Result<(), HandlerError> {
        self.inner.apply(state, ctx, ev).await
    }

    fn supports_idle_timeout(&self) -> bool {
        self.inner.supports_idle_timeout()
    }

    fn pipe_handle(&self) -> Option<PipeHandle<P>> {
        self.inner.pipe_handle()
    }
}
