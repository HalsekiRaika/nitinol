mod combine;
mod default;
mod fused;
mod message;
mod pipe;
mod stash;
mod waker;

pub use self::combine::Combine;
pub use self::default::DefaultDriver;
pub use self::pipe::{PipeDriver, PipeHandle, PipePanic, PipedTask};
pub use self::waker::ProcessWaker;

pub(crate) use self::fused::FusedDriver;
pub(crate) use self::message::MessageDriver;
pub(crate) use self::stash::{StashDriver, StashHandle};

use std::future::Future;

use crate::error::HandlerError;
use crate::process::{Process, ProcessContext};

/// Abstraction over the source that drives a process's lifecycle loop.
///
/// The process's lifecycle loop is generic over `D: Driver<P>` so that any
/// "driving source" — message channels, timer ticks, external futures — can
/// ride the same supervision / registry / dead-letter machinery.
///
/// `next` does NOT borrow `&mut state`, so it can be awaited inside
/// `tokio::select!` alongside the system-signal receiver. `apply` borrows
/// `&mut state` and handles exactly one delivered event.
///
/// Multiple Drivers can be composed into one via [`Combine`] (and the
/// [`combine_drivers!`](crate::combine_drivers) macro). The lifecycle loop
/// still drives a single Driver — multiplexing is the Driver tree's job.
///
/// `ProcessSystem::spawn(props)` automatically composes the following drivers:
/// - `MessageDriver` — mailbox receive (`tell` / `ask`)
/// - `PipeDriver`    — backs `ctx.pipe_to_self`
/// - `StashDriver`   — backs `ctx.stash` / `ctx.unstash_all`
///
/// The driver installed via [`Props::with_driver`](crate::process::Props::with_driver)
/// occupies a single slot composed on top of the three built-in drivers
/// (`MessageDriver`, `PipeDriver`, `StashDriver`). For multi-driver
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
    /// override to `false` because "idle" has no meaning for a scheduled
    /// source.
    fn supports_idle_timeout(&self) -> bool {
        true
    }

    /// The pipe-to-self handle this driver contributes, if any.
    ///
    /// `None` (the default) means this driver does not service
    /// `ProcessContext::pipe_to_self`. `PipeDriver` overrides this to return
    /// `Some(_)`, and `Combine<L, R>` propagates the first non-`None` handle
    /// it finds (preferring the left child). The lifecycle loop reads this
    /// during process startup to wire `ctx.pipe_to_self`.
    fn pipe_handle(&self) -> Option<PipeHandle<P>> {
        None
    }

    /// Hands this driver its process's [`ProcessWaker`].
    ///
    /// The lifecycle calls this once, on the composed driver tree, before the
    /// process's `on_start` and before its loop starts. [`Combine`] hands a
    /// clone to both children, and a driver installed via
    /// [`Props::with_driver`](crate::process::Props::with_driver) receives it
    /// too. The default does nothing, so a driver that ignores the waker keeps
    /// its behaviour.
    ///
    /// A driver that honours the waker keeps it and, in `next`, waits on
    /// whichever of its own wait and [`ProcessWaker::woken`] completes first;
    /// when woken, it delivers one event at once — a tick driver reads now
    /// instead of waiting out its interval.
    ///
    /// The waker is the one [`ProcessContext::waker`] returns, so a process
    /// wakes its own driver from a handler. Below, `Reader` reads on every
    /// tick of `Ticker`, and a `ReadNow` message has it read at once:
    ///
    /// ```
    /// # use std::convert::Infallible;
    /// # use std::time::Duration;
    /// # use nitinol_runtime::error::HandlerError;
    /// # use nitinol_runtime::process::{Driver, Process, ProcessContext, ProcessWaker, Receive};
    /// struct Reader;
    ///
    /// impl Process for Reader {}
    ///
    /// struct Ticker {
    ///     interval: Duration,
    ///     waker: Option<ProcessWaker>,
    /// }
    ///
    /// impl Driver<Reader> for Ticker {
    ///     type Event = ();
    ///
    ///     async fn next(&mut self) -> Option<()> {
    ///         let sleep = tokio::time::sleep(self.interval);
    ///         match &self.waker {
    ///             Some(waker) => tokio::select! {
    ///                 _ = sleep => {}
    ///                 _ = waker.woken() => {}
    ///             },
    ///             None => sleep.await,
    ///         }
    ///         Some(())
    ///     }
    ///
    ///     async fn apply(
    ///         &mut self,
    ///         _state: &mut Reader,
    ///         _ctx: &mut ProcessContext<Reader>,
    ///         _ev: (),
    ///     ) -> Result<(), HandlerError> {
    ///         // read
    ///         Ok(())
    ///     }
    ///
    ///     fn attach_waker(&mut self, waker: ProcessWaker) {
    ///         self.waker = Some(waker);
    ///     }
    /// }
    ///
    /// struct ReadNow;
    ///
    /// impl Receive<ReadNow> for Reader {
    ///     type Response = ();
    ///     type Error = Infallible;
    ///
    ///     async fn recv(
    ///         &mut self,
    ///         _msg: ReadNow,
    ///         ctx: &mut ProcessContext<Self>,
    ///     ) -> Result<(), Infallible> {
    ///         ctx.waker().wake();
    ///         Ok(())
    ///     }
    /// }
    /// ```
    ///
    /// Code outside the process does not need the waker: it sends the process
    /// a message like `ReadNow`.
    #[allow(unused_variables)]
    fn attach_waker(&mut self, waker: ProcessWaker) {}
}
