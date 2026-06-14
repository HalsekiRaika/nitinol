use std::future::Future;
use std::pin::Pin;

use crate::ident::Pid;
use crate::process::spawn::SpawnEnv;
use crate::process::{Process, ProcessProxy};

mod sealed {
    pub trait Sealed {}
}

/// Marker trait for values the unified `ProcessSystem::spawn` /
/// `ProcessContext::spawn_child` entry point can accept.
///
/// Implemented by:
/// - `Props<P>`       → `Output = ProcessProxy<P>`
/// - `StreamProps<T>` → `Output = Result<ProcessProxy<Stream<T>>, SpawnError>`
///
/// The trait is **sealed**: only the runtime crate can add implementors.
/// Users interact exclusively with the concrete types `Props` and `StreamProps`.
pub trait Spawnable: sealed::Sealed {
    /// The value returned to the caller after spawning.
    type Output;
}

/// Crate-private dispatch extension that carries `SpawnEnv`-coupled methods.
///
/// Separated from `Spawnable` so that `SpawnEnv` (a crate-private type) does
/// not appear in the public `Spawnable` surface.
pub(crate) trait SpawnDispatch: Spawnable {
    /// Decompose `self` and invoke the appropriate `SpawnEnv` method.
    fn spawn_with<'a>(
        self,
        env: &'a SpawnEnv,
    ) -> Pin<Box<dyn Future<Output = Self::Output> + Send + 'a>>;

    /// Extract the child `Pid` from `output` for cascade-stop bookkeeping.
    ///
    /// Returns `None` when no child Pid is available — for `StreamProps`
    /// that ran into a duplicate topic, the spawn never happened and there
    /// is nothing to track.
    fn child_pid(output: &Self::Output) -> Option<Pid>;
}

impl<P: Process> sealed::Sealed for crate::process::Props<P> {}

impl<P: Process> Spawnable for crate::process::Props<P> {
    type Output = ProcessProxy<P>;
}

impl<P: Process> SpawnDispatch for crate::process::Props<P> {
    fn spawn_with<'a>(
        self,
        env: &'a SpawnEnv,
    ) -> Pin<Box<dyn Future<Output = Self::Output> + Send + 'a>> {
        Box::pin(async move { env.spawn(self).await })
    }

    fn child_pid(output: &Self::Output) -> Option<Pid> {
        Some(output.pid())
    }
}

impl<T: 'static + Send + Sync> sealed::Sealed for crate::process::StreamProps<T> {}
