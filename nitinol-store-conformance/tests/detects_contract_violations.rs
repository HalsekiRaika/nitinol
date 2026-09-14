//! What the kit catches — the clauses, shown failing against stores that break
//! them.
//!
//! ```gherkin
//! Scenario: a store whose numbering order and visibility order come apart
//!   Given a store that withholds its first append until a reader has passed it
//!   When the kit's concurrency verification is run
//!   Then it fails, naming OCC-5
//!
//! Scenario: a store that serves a batch one event short
//!   Given a store that hides the last event of a batch from the first load
//!   When the kit's deterministic verification is run
//!   Then it fails, naming OCC-3
//! ```
//!
//! A suite that only ever passes says nothing about what it measures, and OCC-5
//! is the clause this matters most for: it was added because a backend can
//! break it while every stored byte looks right and no error is raised
//! anywhere.
//!
//! Both faults are constructed rather than raced. The withheld append waits on
//! the reader's own progress, and the short batch is short on the first load
//! whatever the scheduling, so neither test rests on winning a race — which is
//! also why a probabilistic stress run is not on its own evidence that the kit
//! would have caught anything.
//!
//! `expected` matches the OCC label the kit puts at the front of its message,
//! which is what tells a broken clause from a panic for any other reason.

#[path = "common/helpers.rs"]
mod common;
use common::{Fault, FaultyStore, TokioExecutor};

use nitinol_store_conformance::{verify_deterministic, verify_under_concurrency};

/// A `global_sequence` handed out before the commit it belongs to becomes
/// visible below a checkpoint the reader has already passed, so the event is
/// never read and nothing reports it. The concurrency clause must name that
/// silent loss OCC-5.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[should_panic(expected = "OCC-5")]
async fn delayed_visibility_is_reported_as_an_occ_5_violation() {
    verify_under_concurrency(
        &FaultyStore::injecting(Fault::DelayFirstAppendVisibility),
        &TokioExecutor,
    )
    .await;
}

/// A batch that becomes visible one event short breaks OCC-3's commit unit, and
/// no concurrency is needed to see it: the single-event appends of the earlier
/// clauses go through untouched, so the first clause to fail is the one that
/// reads a batch back.
#[tokio::test]
#[should_panic(expected = "OCC-3")]
async fn a_partially_visible_batch_is_reported_as_an_occ_3_violation() {
    verify_deterministic(&FaultyStore::injecting(
        Fault::WithholdLastEventOfMultiEventBatch,
    ))
    .await;
}
