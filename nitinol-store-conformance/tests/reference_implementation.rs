//! The kit, run against the reference implementations the store contracts are
//! written against.
//!
//! ```gherkin
//! Scenario: the reference event store conforms
//!   Given InMemoryEventStore
//!   When the kit's OCC-1 … OCC-5 verification is run
//!   Then every clause holds
//!
//! Scenario: the reference snapshot store conforms
//!   Given InMemorySnapshotStore
//!   When the kit's SNAP-1 … SNAP-4 verification is run
//!   Then every clause holds
//!
//! Scenario: the reference checkpoint store conforms
//!   Given InMemoryCheckpointStore
//!   When the kit's CKP verification is run
//!   Then every clause holds
//! ```
//!
//! The in-memory stores are the implementations the clauses on
//! `EventStore::append`, `SnapshotStore` and `CheckpointStore` are written
//! against, so a clause that cannot pass here is a clause no backend could be
//! asked to keep. The kit names the clause it found broken when it panics, so a
//! failure reads as the label that was violated rather than as an assertion in
//! this file.
//!
//! The stores and the executor are defined in this crate rather than the kit's,
//! which is the same reach a provider has: nothing here uses anything a
//! third-party backend could not.
//!
//! What a green run here is not: evidence about anything but the contract of
//! the store it was run against. A store put through these clauses has been
//! shown to keep its own contract alone. Whether a projection's result and the
//! checkpoint covering it are settled together is a contract of a store *and*
//! the machinery driving it, and it is fixed elsewhere — see the kit's crate
//! documentation for where.
//!
//! Running the suite needs no feature flag and no environment: a clause that
//! only holds under an opt-in configuration is not a clause this kit keeps.

#[path = "common/helpers.rs"]
mod common;
use common::{ReferenceCheckpointStore, ReferenceSnapshotStore, ReferenceStore, TokioExecutor};

use nitinol_store_conformance::{
    checkpoint, snapshot, verify, verify_deterministic, verify_under_concurrency,
};

/// Every clause — the deterministic ones and the concurrent one — must hold for
/// the reference implementation.
///
/// The multi-threaded runtime is what keeps the concurrent clause from being a
/// sequential test wearing a spawn: on a single-threaded executor the writers
/// would only ever run where the reader yielded.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_in_memory_store_conforms_to_every_occ_clause() {
    verify(&ReferenceStore, &TokioExecutor).await;
}

/// The deterministic half runs on its own and asks for no executor.
///
/// It takes no argument beyond the store, which is the point: the invariants a
/// bad interleaving is not needed for are pinned without any concurrency
/// machinery at all, on a single-threaded runtime that could not hide one.
#[tokio::test]
async fn the_deterministic_clauses_run_without_an_executor() {
    verify_deterministic(&ReferenceStore).await;
}

/// The concurrency half runs on its own, so a provider can put a backend under
/// the stress clause without re-running the deterministic ones — and so that a
/// pass here is never mistaken for the deterministic evidence, which is the
/// other entry's to give.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_concurrency_clauses_run_on_their_own() {
    verify_under_concurrency(&ReferenceStore, &TokioExecutor).await;
}

/// Every `SNAP` clause must hold for the reference snapshot store.
///
/// The entry point asks for no executor and this runs on a single-threaded
/// runtime, which is the whole of the snapshot contract's shape: the
/// framework's only consumer of a snapshot store serialises every call through
/// one process, so there is no clause a concurrent half would be needed for.
#[tokio::test]
async fn the_in_memory_snapshot_store_conforms_to_every_snapshot_clause() {
    snapshot::verify(&ReferenceSnapshotStore).await;
}

/// Every `CKP` clause — the deterministic ones and the concurrent one — must
/// hold for the reference checkpoint store.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_in_memory_checkpoint_store_conforms_to_every_checkpoint_clause() {
    checkpoint::verify(&ReferenceCheckpointStore, &TokioExecutor).await;
}

/// The deterministic checkpoint clauses run on their own and ask for no
/// executor, the same division the OCC half keeps.
#[tokio::test]
async fn the_deterministic_checkpoint_clauses_run_without_an_executor() {
    checkpoint::verify_deterministic(&ReferenceCheckpointStore).await;
}

/// The concurrent checkpoint clause runs on its own, so a provider can put a
/// backend under it without re-running the deterministic ones — and so that a
/// pass here is never mistaken for the deterministic evidence.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_checkpoint_concurrency_clause_runs_on_its_own() {
    checkpoint::verify_under_concurrency(&ReferenceCheckpointStore, &TokioExecutor).await;
}
