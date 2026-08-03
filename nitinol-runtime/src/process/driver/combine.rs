use futures_util::future::Either;

use crate::error::HandlerError;
use crate::process::driver::{Driver, PipeHandle};
use crate::process::{Process, ProcessContext};

/// Binary composition of two [`Driver`]s into one.
///
/// `Combine<L, R>` multiplexes events from `L` and `R` into a single
/// `Driver<P>` so the lifecycle loop sees exactly one source. Each delivered
/// event is tagged via [`Either`] and dispatched back to its originating
/// child's `apply`.
///
/// Use the [`combine_drivers!`](crate::combine_drivers) macro to assemble
/// 2+ Drivers at the call site; it right-folds into nested `Combine`s
/// (`combine_drivers!(A, B, C)` → `Combine::new(A, Combine::new(B, C))`).
///
/// ## `supports_idle_timeout`
///
/// AND-combined across children: any child opting out (`false`) poisons
/// the combined result. This preserves the rule that tick / poll drivers
/// (e.g. `IntervalDriver`) disable the idle timer for the whole loop.
///
/// ## `pipe_handle`
///
/// Propagates the first non-`None` child handle, preferring the left.
/// This is how `combine_drivers!(MessageDriver, PipeDriver::new())` (and
/// deeper trees) surface the pipe handle to the lifecycle loop.
pub struct Combine<L, R> {
    left: L,
    right: R,
}

impl<L, R> Combine<L, R> {
    pub fn new(left: L, right: R) -> Self {
        Self { left, right }
    }
}

impl<P, L, R> Driver<P> for Combine<L, R>
where
    P: Process,
    L: Driver<P>,
    R: Driver<P>,
{
    type Event = Either<L::Event, R::Event>;

    async fn next(&mut self) -> Option<Self::Event> {
        tokio::select! {
            biased;
            ev = self.left.next() => ev.map(Either::Left),
            ev = self.right.next() => ev.map(Either::Right),
        }
    }

    async fn apply(
        &mut self,
        state: &mut P,
        ctx: &mut ProcessContext<P>,
        ev: Self::Event,
    ) -> Result<(), HandlerError> {
        match ev {
            Either::Left(ev) => self.left.apply(state, ctx, ev).await,
            Either::Right(ev) => self.right.apply(state, ctx, ev).await,
        }
    }

    fn supports_idle_timeout(&self) -> bool {
        self.left.supports_idle_timeout() && self.right.supports_idle_timeout()
    }

    fn pipe_handle(&self) -> Option<PipeHandle<P>> {
        self.left.pipe_handle().or_else(|| self.right.pipe_handle())
    }
}

/// Build a single Driver from 1+ child Drivers.
///
/// - `combine_drivers!(A)` is identity (no wrapping).
/// - `combine_drivers!(A, B)` is `Combine::new(A, B)`.
/// - `combine_drivers!(A, B, C, ...)` right-folds:
///   `Combine::new(A, Combine::new(B, Combine::new(C, ...)))`.
///
/// The macro is `#[macro_export]`ed at the crate root.
#[macro_export]
macro_rules! combine_drivers {
    ($single:expr $(,)?) => {
        $single
    };
    ($head:expr, $($rest:expr),+ $(,)?) => {
        $crate::process::Combine::new(
            $head,
            $crate::combine_drivers!($($rest),+),
        )
    };
}
