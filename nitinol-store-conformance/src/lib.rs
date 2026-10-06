//! The contracts every nitinol store owes, made executable for any backend.
//!
//! [`EventStore::append`](nitinol_persistence::store::EventStore::append)
//! states the optimistic-concurrency-control contract as clauses `OCC-1` …
//! `OCC-5`, and
//! [`EventStore::load`](nitinol_persistence::store::EventStore::load) states as
//! `LOAD-1` what a query naming more than one condition means.
//! [`SnapshotStore`](nitinol_persistence::store::SnapshotStore) states
//! `SNAP-1` … `SNAP-4` and
//! [`CheckpointStore`](nitinol_persistence::store::CheckpointStore) states
//! `CKP-1` … `CKP-6`.  Prose binds a backend only as far as someone reads it,
//! so this crate turns those clauses into a suite you can run against yours —
//! the store contracts' counterpart to what `nitinol-conformance` is for the
//! interpreter contract, and to what Akka's persistence TCK is for a plugin
//! author.
//!
//! # Where each contract's clauses are
//!
//! | Contract | Entry points |
//! |---|---|
//! | `EventStore` (`OCC-1` … `OCC-5`, `LOAD-1`) | [`verify`], [`verify_deterministic`], [`verify_under_concurrency`] |
//! | `SnapshotStore` (`SNAP-1` … `SNAP-4`) | [`snapshot::verify`] |
//! | `CheckpointStore` (`CKP-1` … `CKP-6`) | [`checkpoint::verify`], [`checkpoint::verify_deterministic`], [`checkpoint::verify_under_concurrency`] |
//!
//! The three are independent: a backend implementing one of the traits runs
//! that contract's entry points and owes the others nothing.  Two clauses of
//! the checkpoint contract are deliberately not fixed here — the trait's
//! documentation says which, and why, next to each.
//!
//! # What a green run is not
//!
//! Evidence about anything but the store it was run against.  Every clause in
//! this crate is one a backend keeps on its own: what it stored, what it
//! answered, what it refused.  A contract that only holds for a store
//! *together with* the machinery driving it — that a projection's result and
//! the checkpoint covering it are settled in one transaction, and that a
//! conflict takes back both — is not stated here and is not run here.  Those
//! are fixed by `providers/nitinol-sqlx/tests/exactly_once_projection.rs` and
//! `nitinol-eventsource/tests/tx_wiring.rs`, against a provider and a projector
//! at once.  The same holds on the snapshot side: that an aggregate's restored
//! state and the events replayed above it add up to where that aggregate had
//! got to is fixed by `nitinol-eventsource/tests/e2e_snapshot.rs`, against a
//! store and an activating aggregate at once.  A green run of this suite is not
//! a green run of those.
//!
//! ```rust,ignore
//! # use std::sync::Arc;
//! # use futures_core::future::BoxFuture;
//! # use nitinol_persistence::store::EventStore;
//! # use nitinol_store_conformance::{Executor, StoreUnderTest};
//! # struct MyBackend;
//! impl StoreUnderTest for MyBackend {
//!     fn bring_up(&self) -> BoxFuture<'_, Arc<dyn EventStore>> {
//!         Box::pin(async { /* a store of yours holding nothing */ })
//!     }
//! }
//!
//! struct MyRuntime;
//!
//! impl Executor for MyRuntime {
//!     fn spawn(&self, task: BoxFuture<'static, ()>) -> BoxFuture<'static, ()> {
//!         let handle = tokio::spawn(task);
//!         Box::pin(async move { handle.await.expect("a spawned writer must run to completion") })
//!     }
//!
//!     fn yield_now(&self) -> BoxFuture<'static, ()> {
//!         Box::pin(tokio::task::yield_now())
//!     }
//! }
//!
//! #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
//! async fn my_backend_conforms() {
//!     nitinol_store_conformance::verify(&MyBackend, &MyRuntime).await;
//! }
//! ```
//!
//! # The event-store entry points, and why they are not one
//!
//! [`verify_deterministic`] runs the clauses that need no particular
//! interleaving.  Each of them constructs the situation it judges, so what it
//! reports is about the store rather than about how a run happened to be
//! scheduled.  It asks for no executor, and nothing it does is concurrent.
//!
//! [`verify_under_concurrency`] runs the one clause that does need a reader and
//! writers at the same time.  A pass there says a run of that shape found
//! nothing — weaker evidence than the deterministic half gives, and kept
//! separate so the two are never mistaken for each other.  A backend that has
//! only been through this half has not been through the suite.
//!
//! [`verify`] is both, in that order.
//!
//! The [`checkpoint`] module keeps the same division for the same reason.  The
//! [`snapshot`] module has one entry point and no concurrency half, because the
//! snapshot contract has no clause that needs one; the module documentation
//! says why.
//!
//! # Running it against a real backend
//!
//! Two things a provider owes the suite, beyond implementing the under-test
//! trait of the contract it is running — [`StoreUnderTest`],
//! [`snapshot::SnapshotStoreUnderTest`] or
//! [`checkpoint::CheckpointStoreUnderTest`]:
//!
//! **`bring_up` must hand back a store holding nothing.** Every clause asks for
//! its own, and a clause reading another's leftovers is judging something other
//! than what it staged.
//!
//! **The writers must reach the backend independently.** The concurrency
//! clauses spawn them through your [`Executor`]; if they then queue behind one
//! connection, the run is sequential and proves nothing about concurrency.  A
//! green run of that shape is not evidence.
//!
//! # What this crate may not depend on
//!
//! No async runtime.  A kit that reached for one would be out of reach of every
//! provider built on another, so whichever runtime you await these entry points
//! on stays yours to choose, and [`Executor`] is how the suite borrows it.

pub mod checkpoint;
pub mod snapshot;

mod concurrency;
mod deterministic;
mod fixture;
mod under_test;

pub use self::concurrency::verify_under_concurrency;
pub use self::deterministic::verify_deterministic;
pub use self::under_test::{Executor, StoreUnderTest};

/// Put an event store through every clause of the event-store contract.
///
/// The deterministic clauses run first, so a store that breaks one of them is
/// reported on what it broke rather than on the concurrent reader that then
/// could not keep up with it.
pub async fn verify<S: StoreUnderTest, E: Executor>(store: &S, executor: &E) {
    verify_deterministic(store).await;
    verify_under_concurrency(store, executor).await;
}
