use std::future::Future;

use crate::error::HandlerError;
use crate::process::driver::{Driver, DynDriver, PipeHandle};
use crate::process::{Process, ProcessContext};

/// `Driver<P>` over a `Vec<Box<dyn DynDriver<P>>>` — the runtime form of
/// `Props::add_driver`'s accumulated custom drivers.
///
/// The lifecycle loop sees a single driver, but internally `DynDriverSet`
/// races every child's `next_dyn` via `select_all`, records which one fired,
/// and routes the subsequent `apply` to the same index. Heterogeneous
/// `Event` types stay encapsulated inside each `DynDriver` so the set's own
/// `Event` is just the index of the firing child.
///
/// An empty set never returns `None` — that would stop the actor, which is
/// the wrong contract for "no custom drivers configured". The Core trio
/// (`MessageDriver` / `PipeDriver` / `StashDriver`) drives termination.
pub(crate) struct DynDriverSet<P: Process> {
    drivers: Vec<Box<dyn DynDriver<P>>>,
}

impl<P: Process> DynDriverSet<P> {
    pub(crate) fn new(drivers: Vec<Box<dyn DynDriver<P>>>) -> Self {
        Self { drivers }
    }
}

impl<P: Process> Driver<P> for DynDriverSet<P> {
    type Event = usize;

    fn next(&mut self) -> impl Future<Output = Option<Self::Event>> + Send {
        async move {
            loop {
                if self.drivers.is_empty() {
                    // Hang forever rather than reporting None — the Core trio
                    // owns process termination.
                    std::future::pending::<()>().await;
                    unreachable!()
                }
                // Build a fresh BoxFuture per child for this `next()` call and
                // race them via `select_all`. The futures stay alive across
                // internal polls inside `select_all`, so sources like
                // `tokio::time::sleep` are NOT restarted each waker firing.
                let futures: Vec<_> = self.drivers.iter_mut().map(|d| d.next_dyn()).collect();
                let (result, idx, remaining) = futures_util::future::select_all(futures).await;
                // Drop `remaining` before any mutation of `self.drivers` so
                // the borrow checker sees that the mutable borrows from
                // `iter_mut()` are released before `swap_remove`.
                drop(remaining);
                match result {
                    Some(()) => return Some(idx),
                    None => {
                        // Driver at `idx` is exhausted — remove it and retry
                        // with the remaining drivers so the Core trio stays
                        // alive. swap_remove is O(1); ordering does not matter
                        // because `futures` is rebuilt from scratch each iteration.
                        self.drivers.swap_remove(idx);
                    }
                }
            }
        }
    }

    async fn apply(
        &mut self,
        state: &mut P,
        ctx: &mut ProcessContext<P>,
        ev: Self::Event,
    ) -> Result<(), HandlerError> {
        self.drivers[ev].apply_dyn(state, ctx).await
    }

    fn supports_idle_timeout(&self) -> bool {
        // AND across children: if any custom driver opts out, the whole set
        // disarms the timer — same rule as `Combine`. An empty set defaults
        // to `true` so it does not poison the Core idle behavior.
        self.drivers.iter().all(|d| d.supports_idle_timeout())
    }

    fn pipe_handle(&self) -> Option<PipeHandle<P>> {
        // Custom drivers never own the Pipe channel; Core's `PipeDriver`
        // surfaces it via `Combine` in lifecycle::run.
        None
    }
}
