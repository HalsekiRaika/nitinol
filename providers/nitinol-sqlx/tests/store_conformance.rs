//! The OCC contract, run against the Postgres provider.
//!
//! ```gherkin
//! Scenario: the provider keeps every OCC clause
//!   Given a Postgres this run has reached
//!   When the conformance suite's OCC-1 … OCC-5 verification is run against it
//!   Then every clause holds
//!
//! Scenario: an offset reader misses nothing under concurrent appends
//!   Given several connections appending to streams of their own at once
//!   And a reader following the global order by offset
//!   When the appends have finished and the reader has caught up
//!   Then the reader has observed every committed event exactly once
//! ```
//!
//! The suite is the store contract's executable half and is not reimplemented
//! here: a backend that has not been through it has not been shown to keep the
//! contract, and a backend put only through the concurrency clause has not been
//! through the suite.
//!
//! Running it needs a database. Where none can be reached the run judges
//! nothing and says so, which is not evidence of conformance.

#[path = "common/harness.rs"]
mod harness;

use harness::TokioExecutor;
use nitinol_store_conformance::{verify, verify_under_concurrency};

/// Every clause — the deterministic ones and the concurrent one.
///
/// The multi-threaded runtime is what keeps the concurrent clause from being a
/// sequential test wearing a spawn: on a single-threaded executor the writers
/// would only ever run where the reader yielded.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_postgres_store_conforms_to_every_occ_clause() {
    let Some(backend) = harness::postgres_backend().await else {
        return;
    };

    verify(&backend, &TokioExecutor).await;
}

/// OCC-5 on its own, against a store of its own.
///
/// The clause is the reason this provider serialises numbering with the commit
/// it belongs to, and it is the one clause nothing else notices the breaking
/// of: the writer succeeds, the reader raises nothing, and the event is read by
/// nobody.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_offset_reader_observes_every_committed_event_exactly_once() {
    let Some(backend) = harness::postgres_backend().await else {
        return;
    };

    verify_under_concurrency(&backend, &TokioExecutor).await;
}
