//! The contract every `CheckpointStore` owes, made executable for any backend.
//!
//! [`CheckpointStore`] states it as clauses `CKP-1` … `CKP-6`; this module is
//! the ones a suite can fix as something to run. Two of them it deliberately
//! does not fix — `CKP-4`, where the contract leaves two answers open, and
//! `CKP-6`, which is about a transaction a kit cannot make — and
//! [`CheckpointStore`] says so next to each.
//!
//! # Three entry points, and why they are not one
//!
//! [`verify_deterministic`] runs the clauses that need no particular
//! interleaving. Each of them constructs the situation it judges, so what it
//! reports is about the store rather than about how a run happened to be
//! scheduled. It asks for no executor, and nothing it does is concurrent.
//!
//! [`verify_under_concurrency`] runs the one clause that needs several writers
//! recording at the same time — which is how a deployment reaches a checkpoint
//! store, the partitions of a delivery host each keeping progress of their own.
//! A pass there says a run of that shape found nothing, weaker evidence than
//! the deterministic half gives, and kept separate so the two are never
//! mistaken for each other.
//!
//! [`verify`] is both, in that order.
//!
//! # What a green run here is not
//!
//! Evidence about anything but the store alone. Whether a projection's result
//! and the checkpoint covering it are settled together is a contract of a
//! store *and* the machinery driving it; see the
//! [crate documentation](crate) for where the combined contracts are fixed.

mod concurrency;
mod deterministic;
pub(crate) mod fixture;

use std::sync::Arc;

use futures_core::future::BoxFuture;
use nitinol_persistence::store::CheckpointStore;

pub use self::concurrency::verify_under_concurrency;
pub use self::deterministic::verify_deterministic;

use crate::under_test::Executor;

/// The checkpoint backend a run of the `CKP` clauses is about.
///
/// The store is an associated type rather than a `dyn` handle:
/// [`CheckpointStore::Tx`] is an associated type of its own, and what a
/// transaction is is the one thing a checkpoint backend decides for itself.
///
/// Every clause asks for its own store, so no clause can be carried by what
/// another one left behind, and a provider decides for itself what a store
/// holding nothing means for its backend — a fresh schema, a truncated table, a
/// pool of its own.
///
/// `Send + Sync` so that the future a clause returns is itself `Send`: a
/// provider running the suite as an integration test may want to spawn it.
pub trait CheckpointStoreUnderTest: Send + Sync {
    /// The backend's own store type.
    type Store: CheckpointStore + 'static;

    /// Hand the suite a store holding nothing.
    fn bring_up(&self) -> BoxFuture<'_, Arc<Self::Store>>;
}

/// Put a checkpoint store through every clause the suite fixes.
///
/// The deterministic clauses run first, so a store that breaks one of them is
/// reported on what it broke rather than on the concurrent writers that then
/// could not record against it.
pub async fn verify<S: CheckpointStoreUnderTest, E: Executor>(store: &S, executor: &E) {
    verify_deterministic(store).await;
    verify_under_concurrency(store, executor).await;
}
