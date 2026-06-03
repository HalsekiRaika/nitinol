use std::any::Any;
use std::fmt;

use futures_util::future::BoxFuture;
use futures_util::stream::{FuturesUnordered, StreamExt};
use tokio::sync::mpsc;

use crate::error::HandlerError;
use crate::process::driver::Driver;
use crate::process::task::UserTask;
use crate::process::{Process, ProcessContext};

/// Opaque envelope carrying a piped task from the `FuturesUnordered` queue
/// to the driver's `apply`.
///
/// Users never construct or inspect `PipedTask`; it exists solely so that
/// the public `Driver::Event` associated type does not leak the
/// crate-private `Task<P>` trait. `ctx.pipe_to_self` builds these
/// internally; `PipeDriver::apply` consumes them.
pub struct PipedTask<P: Process> {
    inner: UserTask<P>,
}

impl<P: Process> PipedTask<P> {
    pub(crate) fn new(task: UserTask<P>) -> Self {
        Self { inner: task }
    }
}

/// Future enqueued by `ProcessContext::pipe_to_self`.
///
/// Each pipe future resolves to a [`PipedTask<P>`] which the driver's
/// `apply` then runs against the actor state. Keeping the boxing inside
/// `pipe_to_self` lets `PipeDriver` stay free of any knowledge about `M`
/// (only the task type matters here).
pub(crate) type PipeFuture<P> = BoxFuture<'static, PipedTask<P>>;

/// Driver that delivers the results of `ctx.pipe_to_self(fut, mapper)` back
/// into the actor as ordinary `Receive` messages.
///
/// In-flight futures are owned by a [`FuturesUnordered`] that is polled
/// **inside the lifecycle loop's single task** — the actor model's
/// single-threaded guarantee is preserved. `PipeDriver` never calls
/// `tokio::spawn`.
///
/// ## CPU-bound work is unsupported
///
/// A pipe future that blocks the runtime (long CPU-bound computation,
/// blocking syscalls) will stall the entire lifecycle loop: messages,
/// system signals, supervision timers — everything. This is the same
/// anti-pattern as blocking inside `Receive::recv` and the runtime makes
/// no attempt to detect or recover from it. Offload such work to
/// `tokio::task::spawn_blocking` and pipe only the resulting future.
///
/// ## Panic capture
///
/// Each pipe future is polled inside an `AssertUnwindSafe(...).catch_unwind()`
/// wrapper (set up by `pipe_to_self`); a panic during poll is converted to
/// [`PipePanic`] and handed to the mapper as `Err(PipePanic)`. The actor
/// continues running.
///
/// ## No abort handle
///
/// Pipe futures cannot be cancelled individually. They are dropped
/// collectively when the lifecycle loop ends.
pub struct PipeDriver<P: Process> {
    // Keep our own clone of the sender so the channel does not close
    // even when no `PipeHandle` is currently alive elsewhere.
    tx: mpsc::UnboundedSender<PipeFuture<P>>,
    rx: mpsc::UnboundedReceiver<PipeFuture<P>>,
    in_flight: FuturesUnordered<PipeFuture<P>>,
}

impl<P: Process> PipeDriver<P> {
    pub fn new() -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        Self {
            tx,
            rx,
            in_flight: FuturesUnordered::new(),
        }
    }
}

impl<P: Process> Default for PipeDriver<P> {
    fn default() -> Self {
        Self::new()
    }
}

impl<P: Process> Driver<P> for PipeDriver<P> {
    type Event = PipedTask<P>;

    async fn next(&mut self) -> Option<Self::Event> {
        loop {
            tokio::select! {
                biased;
                Some(fut) = self.rx.recv() => {
                    self.in_flight.push(fut);
                }
                Some(task) = self.in_flight.next(), if !self.in_flight.is_empty() => {
                    return Some(task);
                }
                else => return None,
            }
        }
    }

    async fn apply(
        &mut self,
        state: &mut P,
        ctx: &mut ProcessContext<P>,
        ev: Self::Event,
    ) -> Result<(), HandlerError> {
        ev.inner.run(state, ctx).await
    }

    fn pipe_handle(&self) -> Option<PipeHandle<P>> {
        Some(PipeHandle {
            tx: self.tx.clone(),
        })
    }
}

/// Handle used by [`ProcessContext::pipe_to_self`] to enqueue a future onto
/// the owning process's `PipeDriver`.
///
/// Surfaced to the lifecycle loop via [`Driver::pipe_handle`]. Not a
/// user-facing type — users interact only through `ctx.pipe_to_self(...)`.
pub struct PipeHandle<P: Process> {
    pub(crate) tx: mpsc::UnboundedSender<PipeFuture<P>>,
}

// Manual `Clone` impl: `#[derive(Clone)]` would (incorrectly) require
// `P: Clone`. Cloning only clones the sender.
impl<P: Process> Clone for PipeHandle<P> {
    fn clone(&self) -> Self {
        Self {
            tx: self.tx.clone(),
        }
    }
}

/// Wrapper around a panic payload captured from a piped future.
///
/// Reaches the user as the `Err` arm of the mapper's input. Both `Display`
/// and the `payload()` accessor are provided so callers can either log the
/// panic message or downcast to a known concrete type.
pub struct PipePanic {
    payload: Box<dyn Any + Send + 'static>,
}

impl PipePanic {
    pub(crate) fn new(payload: Box<dyn Any + Send + 'static>) -> Self {
        Self { payload }
    }

    /// Raw panic payload as returned by `catch_unwind`.
    ///
    /// Downcast to the concrete type if known
    /// (`payload().downcast_ref::<String>()` etc.). Most std-library
    /// panics carry either `&'static str` or `String` — those are the two
    /// shapes `Display` formats; everything else falls back to a
    /// `"<non-string panic payload>"` placeholder so logs stay non-empty.
    pub fn payload(&self) -> &(dyn Any + Send + 'static) {
        self.payload.as_ref()
    }
}

impl fmt::Display for PipePanic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(s) = self.payload.downcast_ref::<&'static str>() {
            f.write_str(s)
        } else if let Some(s) = self.payload.downcast_ref::<String>() {
            f.write_str(s)
        } else {
            f.write_str("<non-string panic payload>")
        }
    }
}

impl fmt::Debug for PipePanic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "PipePanic({})", self)
    }
}
