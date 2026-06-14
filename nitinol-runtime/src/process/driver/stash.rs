use std::future::Future;
use std::marker::PhantomData;
use std::num::NonZeroUsize;

use crate::error::HandlerError;
use crate::process::driver::Driver;
use crate::process::{Process, ProcessContext};

/// Driver mount point for the stash machinery.
///
/// Issue #56 reserves the API surface and the spawn-time slot in the Core
/// driver tree. The full stash queue / replay implementation lives in a
/// follow-up issue ("the B issue"); for now this driver:
/// - accepts the configured `StashCapacity` so the spawn boundary can finish
///   resolving `Inherit` once and pass a concrete `NonZeroUsize` down;
/// - reports `supports_idle_timeout = true` so it does not interfere with
///   `IdleTimeout::After` handling;
/// - pends forever inside `next` so the lifecycle loop never sees an event
///   from this driver (matches the stub `ctx.stash` / `ctx.unstash_all`).
pub(crate) struct StashDriver<P: Process> {
    #[allow(dead_code)]
    capacity: NonZeroUsize,
    _phantom: PhantomData<fn(P)>,
}

impl<P: Process> StashDriver<P> {
    pub(crate) fn new(capacity: NonZeroUsize) -> Self {
        Self {
            capacity,
            _phantom: PhantomData,
        }
    }
}

impl<P: Process> Driver<P> for StashDriver<P> {
    type Event = std::convert::Infallible;

    fn next(&mut self) -> impl Future<Output = Option<Self::Event>> + Send {
        // Stub: yield no events. The lifecycle loop's `select!` will simply
        // never pick this branch.
        std::future::pending()
    }

    async fn apply(
        &mut self,
        _state: &mut P,
        _ctx: &mut ProcessContext<P>,
        ev: Self::Event,
    ) -> Result<(), HandlerError> {
        // `Infallible` cannot be constructed — `apply` is unreachable until
        // the B issue lands the queue/replay implementation.
        match ev {}
    }
}
