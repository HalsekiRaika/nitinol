mod message;

pub(crate) use self::message::MessageDriver;

use std::future::Future;

use crate::error::HandlerError;
use crate::process::{Process, ProcessContext};

/// Abstraction over the source that drives a process's lifecycle loop.
///
/// `lifecycle_loop` is generic over `D: Driver<P>` so that any "driving
/// source" — message channels, timer ticks, external futures — can ride the
/// same supervision / registry / dead-letter machinery (Issue #48).
///
/// `next` does NOT borrow `&mut state`, so it can be awaited inside
/// `tokio::select!` alongside the system-signal receiver. `apply` borrows
/// `&mut state` and handles exactly one delivered event.
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
        ctx: &mut ProcessContext,
        ev: Self::Event,
    ) -> impl Future<Output = Result<(), HandlerError>> + Send;

    /// Whether the lifecycle loop should arm its idle-timeout timer for this
    /// driver. Message-driven drivers default to `true` so existing
    /// `IdleTimeout::After(_)` semantics are preserved. Tick / poll drivers
    /// (e.g. the planned `IntervalDriver` in #49) override to `false` because
    /// "idle" has no meaning for a scheduled source.
    fn supports_idle_timeout(&self) -> bool {
        true
    }
}
