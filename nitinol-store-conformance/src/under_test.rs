use std::sync::Arc;

use futures_core::future::BoxFuture;
use nitinol_persistence::store::EventStore;

/// The backend a run of the suite is about.
///
/// Every clause asks for its own store, so no clause can be carried by what
/// another one left behind, and a provider decides for itself what a store
/// holding nothing means for its backend — a fresh schema, a truncated table, a
/// pool of its own.
///
/// `Send + Sync` so that the future a clause returns is itself `Send`: a
/// provider running the suite as an integration test may want to spawn it.
pub trait StoreUnderTest: Send + Sync {
    /// Hand the suite a store holding nothing.
    fn bring_up(&self) -> BoxFuture<'_, Arc<dyn EventStore>>;
}

/// The caller's runtime, lent to the suite for the clause that needs writers
/// making progress while a reader reads.
///
/// The kit has no runtime of its own on purpose: one that reached for a
/// particular runtime would be out of reach of every provider built on another.
pub trait Executor: Send + Sync {
    /// Start `task`, and return a future that completes when it does.
    ///
    /// `task` must begin making progress without the returned future being
    /// awaited.  The suite starts every writer before it reads anything, and
    /// awaits what this returns only once the writers have stopped, so a
    /// `spawn` that merely handed the task back would leave the reader racing
    /// nobody.
    fn spawn(&self, task: BoxFuture<'static, ()>) -> BoxFuture<'static, ()>;

    /// Give the runtime the chance to run everything else that is ready.
    ///
    /// The reader polls in a loop.  A store whose `load` completes without ever
    /// awaiting would otherwise starve the writers the reader is supposed to be
    /// running against.
    fn yield_now(&self) -> BoxFuture<'static, ()>;
}
