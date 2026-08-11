use std::future::Future;

use crate::error::HandlerError;
use crate::process::driver::Driver;
use crate::process::{Process, ProcessContext};

/// The default driver type parameter for [`Props<P, D>`](crate::process::Props).
///
/// `DefaultDriver` is the type-level sentinel that means **no additional driver
/// has been composed on top of the Core trio** (`MessageDriver`, `PipeDriver`
/// and `StashDriver`). It is not a "no-op" driver in the conceptual sense; it
/// occupies the `D` slot of `Props` so the type can be written without naming
/// a custom driver, and the lifecycle loop composes it next to the Core trio
/// via [`Combine`](crate::process::driver::Combine).
///
/// Multi-driver use cases compose drivers via the
/// [`combine_drivers!`](crate::combine_drivers) macro and install the
/// resulting type via [`Props::with_driver`](crate::process::Props::with_driver).
pub struct DefaultDriver;

impl<P: Process> Driver<P> for DefaultDriver {
    type Event = std::convert::Infallible;

    fn next(&mut self) -> impl Future<Output = Option<Self::Event>> + Send {
        // Contribute no events. `Combine<Core, DefaultDriver>::next` always
        // resolves via the Core side; this branch stays pending forever.
        std::future::pending()
    }

    async fn apply(
        &mut self,
        _state: &mut P,
        _ctx: &mut ProcessContext<P>,
        ev: Self::Event,
    ) -> Result<(), HandlerError> {
        // `Infallible` cannot be constructed, so this branch is unreachable.
        match ev {}
    }
}
