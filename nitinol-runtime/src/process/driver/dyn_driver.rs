use std::marker::PhantomData;

use futures_util::future::BoxFuture;

use crate::error::HandlerError;
use crate::process::driver::Driver;
use crate::process::{Process, ProcessContext};

/// Object-safe shim over [`Driver<P>`] so heterogeneous custom drivers can be
/// stored in a `Vec<Box<dyn DynDriver<P>>>` on `Props<P>`.
///
/// Each implementor buffers its own pending event between `next_dyn` (which
/// signals readiness with `Some(())`) and `apply_dyn` (which consumes the
/// buffered event). The lifecycle loop polls every driver via
/// [`crate::process::driver::DynDriverSet`] and dispatches by index, so each
/// driver's `Event` type can stay unique without leaking through `dyn`.
pub(crate) trait DynDriver<P: Process>: Send + 'static {
    /// Drive the underlying source and stash the resulting event internally.
    /// Returns `Some(())` when an event is buffered (caller should call
    /// `apply_dyn`), `None` when the source is exhausted.
    fn next_dyn<'a>(&'a mut self) -> BoxFuture<'a, Option<()>>;

    /// Run the buffered event against the actor state.
    ///
    /// `panic`s if called without a preceding successful `next_dyn` —
    /// the lifecycle loop always calls them in lock-step, so reaching that
    /// branch is a runtime invariant violation worth surfacing immediately.
    fn apply_dyn<'a>(
        &'a mut self,
        state: &'a mut P,
        ctx: &'a mut ProcessContext<P>,
    ) -> BoxFuture<'a, Result<(), HandlerError>>;

    fn supports_idle_timeout(&self) -> bool;
}

struct DynAdapter<P: Process, D: Driver<P>> {
    inner: D,
    pending: Option<D::Event>,
    _phantom: PhantomData<fn(P)>,
}

impl<P: Process, D: Driver<P>> DynDriver<P> for DynAdapter<P, D> {
    fn next_dyn<'a>(&'a mut self) -> BoxFuture<'a, Option<()>> {
        Box::pin(async move {
            match self.inner.next().await {
                Some(ev) => {
                    self.pending = Some(ev);
                    Some(())
                }
                None => None,
            }
        })
    }

    fn apply_dyn<'a>(
        &'a mut self,
        state: &'a mut P,
        ctx: &'a mut ProcessContext<P>,
    ) -> BoxFuture<'a, Result<(), HandlerError>> {
        Box::pin(async move {
            let ev = self
                .pending
                .take()
                .expect("DynDriver::apply_dyn called without a buffered event");
            self.inner.apply(state, ctx, ev).await
        })
    }

    fn supports_idle_timeout(&self) -> bool {
        self.inner.supports_idle_timeout()
    }
}

/// Box a concrete `Driver<P>` into the object-safe form stored on `Props`.
pub(crate) fn boxed_dyn_driver<P, D>(driver: D) -> Box<dyn DynDriver<P>>
where
    P: Process,
    D: Driver<P>,
{
    Box::new(DynAdapter::<P, D> {
        inner: driver,
        pending: None,
        _phantom: PhantomData,
    })
}
