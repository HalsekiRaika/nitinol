mod combine;
mod default;
mod fused;
mod message;
mod pipe;
mod stash;

pub use self::combine::Combine;
pub use self::default::DefaultDriver;
pub use self::pipe::{PipeDriver, PipeHandle, PipePanic, PipedTask};

pub(crate) use self::fused::FusedDriver;
pub(crate) use self::message::MessageDriver;
pub(crate) use self::stash::{StashDriver, StashHandle};

use std::future::Future;

use crate::error::HandlerError;
use crate::process::{Process, ProcessContext};

/// Abstraction over the source that drives a process's lifecycle loop.
///
/// `lifecycle_loop` is generic over `D: Driver<P>` so that any "driving
/// source" — message channels, timer ticks, external futures — can ride the
/// same supervision / registry / dead-letter machinery.
///
/// `next` does NOT borrow `&mut state`, so it can be awaited inside
/// `tokio::select!` alongside the system-signal receiver. `apply` borrows
/// `&mut state` and handles exactly one delivered event.
///
/// Multiple Drivers can be composed into one via [`Combine`] (and the
/// [`combine_drivers!`](crate::combine_drivers) macro). The lifecycle loop
/// still drives a single Driver — multiplexing is the Driver tree's job.
///
/// # Implicit Drivers
///
/// `ProcessSystem::spawn(props)` automatically composes the following drivers:
/// - `MessageDriver` — mailbox receive (`tell` / `ask`)
/// - `PipeDriver`    — backs `ctx.pipe_to_self`
/// - `StashDriver`   — backs `ctx.stash` / `ctx.unstash_all`
///
/// The driver installed via [`Props::with_driver`](crate::process::Props::with_driver)
/// occupies a single slot composed on top of the Core trio. For multi-driver
/// composition, build a single combined driver with
/// [`combine_drivers!`](crate::combine_drivers) and pass it to `with_driver`.
pub trait Driver<P: Process>: Send + 'static {
    /// One unit of work delivered by `next` and consumed by `apply`.
    type Event: Send;

    /// Await the next event from the underlying source. Returns `None` when
    /// the source is exhausted; the lifecycle loop treats this as
    /// `TerminatedReason::Stopped`.
    fn next(&mut self) -> impl Future<Output = Option<Self::Event>> + Send;

    /// Apply one event to the process state. The returned `Err(HandlerError)`
    /// is observed by the supervision strategy.
    fn apply(
        &mut self,
        state: &mut P,
        ctx: &mut ProcessContext<P>,
        ev: Self::Event,
    ) -> impl Future<Output = Result<(), HandlerError>> + Send;

    /// Whether the lifecycle loop should arm its idle-timeout timer for this
    /// driver. Message-driven drivers default to `true` so existing
    /// `IdleTimeout::After(_)` semantics are preserved. Tick / poll drivers
    /// (e.g. a planned `IntervalDriver`) override to `false` because
    /// "idle" has no meaning for a scheduled source.
    fn supports_idle_timeout(&self) -> bool {
        true
    }

    /// The pipe-to-self handle this driver contributes, if any.
    ///
    /// `None` (the default) means this driver does not service
    /// `ProcessContext::pipe_to_self`. `PipeDriver` overrides this to return
    /// `Some(_)`, and `Combine<L, R>` propagates the first non-`None` handle
    /// it finds (preferring the left child). The lifecycle loop reads this
    /// during process startup to wire `ctx.pipe_to_self` — if the resolved
    /// value is `None`, calling `pipe_to_self` panics with a misuse message
    /// rather than silently dropping the future.
    fn pipe_handle(&self) -> Option<PipeHandle<P>> {
        None
    }
}
