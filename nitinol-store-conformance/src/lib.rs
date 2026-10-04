//! The contract every `EventStore` owes, made executable for any backend.
//!
//! [`EventStore::append`](nitinol_persistence::store::EventStore::append)
//! states the optimistic-concurrency-control contract as clauses `OCC-1` …
//! `OCC-5`, and
//! [`EventStore::load`](nitinol_persistence::store::EventStore::load) states as
//! `LOAD-1` what a query naming more than one condition means.  Prose binds a
//! backend only as far as someone reads it, so this crate turns those clauses
//! into a suite you can run against yours — the store contract's counterpart to
//! what `nitinol-conformance` is for the interpreter contract, and to what
//! Akka's persistence TCK is for a plugin author.
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
//! # Three entry points, and why they are not one
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
//! # Running it against a real backend
//!
//! Two things a provider owes the suite, beyond implementing
//! [`StoreUnderTest`]:
//!
//! **[`bring_up`](StoreUnderTest::bring_up) must hand back a store holding
//! nothing.** Every clause asks for its own, and a clause reading another's
//! leftovers is judging something other than what it staged.
//!
//! **The writers must reach the backend independently.** The concurrency clause
//! spawns them through your [`Executor`]; if they then queue behind one
//! connection, the run is sequential and proves nothing about concurrency.  A
//! green run of that shape is not evidence.
//!
//! # What this crate may not depend on
//!
//! No async runtime.  A kit that reached for one would be out of reach of every
//! provider built on another, so whichever runtime you await these entry points
//! on stays yours to choose, and [`Executor`] is how the suite borrows it.

mod concurrency;
mod deterministic;
mod fixture;
mod under_test;

pub use self::concurrency::verify_under_concurrency;
pub use self::deterministic::verify_deterministic;
pub use self::under_test::{Executor, StoreUnderTest};

/// Put a store through every clause of the store contract.
///
/// The deterministic clauses run first, so a store that breaks one of them is
/// reported on what it broke rather than on the concurrent reader that then
/// could not keep up with it.
pub async fn verify<S: StoreUnderTest, E: Executor>(store: &S, executor: &E) {
    verify_deterministic(store).await;
    verify_under_concurrency(store, executor).await;
}
