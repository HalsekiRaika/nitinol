//! The kit, run against the reference implementation of the OCC contract.
//!
//! ```gherkin
//! Scenario: the reference implementation conforms
//!   Given InMemoryEventStore
//!   When the kit's OCC-1 … OCC-5 verification is run
//!   Then every clause holds
//! ```
//!
//! `InMemoryEventStore` is the implementation the clauses on
//! `EventStore::append` are written against, so a clause that cannot pass here
//! is a clause no backend could be asked to keep. The kit names the clause it
//! found broken when it panics, so a failure reads as the OCC label that was
//! violated rather than as an assertion in this file.
//!
//! The store and the executor are defined in this crate rather than the kit's,
//! which is the same reach a provider has: nothing here uses anything a
//! third-party backend could not.
//!
//! Running the suite needs no feature flag and no environment: a clause that
//! only holds under an opt-in configuration is not a clause this kit keeps.

#[path = "common/helpers.rs"]
mod common;
use common::{ReferenceStore, TokioExecutor};

use nitinol_store_conformance::{verify, verify_deterministic, verify_under_concurrency};

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
