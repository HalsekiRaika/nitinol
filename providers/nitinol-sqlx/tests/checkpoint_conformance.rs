//! The checkpoint contract, run against the Postgres provider.
//!
//! ```gherkin
//! Scenario: the provider keeps every checkpoint clause
//!   Given a Postgres this run has reached
//!   When the conformance suite's CKP verification is run against it
//!   Then every clause holds
//!
//! Scenario: partitions recording at the same time do not disturb each other
//!   Given several connections saving positions under keys of their own at once
//!   When the saves have finished
//!   Then every key holds the last position its own writer saved
//! ```
//!
//! What this fixes and what `checkpoint_store.rs` fixes are different things.
//! The clauses here are the ones *every* backend owes, so they are the suite's
//! and are not restated: a backend that has not been through it has not been
//! shown to keep the contract. The rule this provider adds on top — a checkpoint
//! moves forward or not at all, and a save that would not carry it forward is
//! refused as a conflict a caller can name — is this backend's own and is fixed
//! next to it, because the contract leaves a non-forward save to the backend and
//! the reference implementation answers it the other way.
//!
//! Nor is a green run here evidence about a projection: whether a read-model
//! update and the checkpoint covering it are settled together is a contract of
//! this store *and* the projector driving it, and `exactly_once_projection.rs`
//! is where that is fixed.
//!
//! Running it needs a database. Where none can be reached the run judges
//! nothing and says so, which is not evidence of conformance.

#[path = "common/harness.rs"]
mod harness;

use harness::TokioExecutor;
use nitinol_store_conformance::checkpoint;

/// Every clause — the deterministic ones and the concurrent one.
///
/// The multi-threaded runtime is what keeps the concurrent clause from being a
/// sequential test wearing a spawn: on a single-threaded executor the writers
/// would only ever run where the suite yielded.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_postgres_checkpoint_store_conforms_to_every_checkpoint_clause() {
    let Some(backend) = harness::postgres_backend().await else {
        return;
    };

    checkpoint::verify(&backend, &TokioExecutor).await;
}

/// The concurrent clause on its own, against a store of its own.
///
/// This is the one clause that puts the provider's pool under more than one
/// writer at a time, which is how a deployment actually reaches it: the
/// partitions of a delivery host record their progress from processes of their
/// own, under keys of their own, at the same moment.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_writers_under_keys_of_their_own_do_not_disturb_each_other() {
    let Some(backend) = harness::postgres_backend().await else {
        return;
    };

    checkpoint::verify_under_concurrency(&backend, &TokioExecutor).await;
}
