//! Where the Postgres journal ends, and what answering it reads.
//!
//! ```gherkin
//!   Scenario: a journal nothing was appended to has no head
//!     Given a freshly migrated schema
//!     When the store's head is asked for
//!     Then it answers that there is none
//!
//!   Scenario: the head is read off the global-order index
//!     Given a journal seeded with thousands of rows across two streams
//!     When the store's head is asked for
//!     Then it is the last global_sequence the store assigned
//!     And it was answered by scanning nitinol_journal_global_sequence_key
//!     And it did not read the journal through
//! ```
//!
//! `HEAD-1` and `HEAD-2` are fixed for this provider by the conformance suite
//! (`store_conformance`). What is pinned here is what the suite does not
//! judge: that a schema holding no rows answers `None` rather than failing on
//! a `MAX` over nothing, and that the head is served by the index the
//! migrations already create, so asking for it does not cost a pass over the
//! journal however long it grows.
//!
//! What is measured is the statement the provider itself issues, read off the
//! database's own counters, for the reason `journal_event_type_index` gives: a
//! copy of the statement would keep passing if the provider's own stopped being
//! able to use the index.
//!
//! Running any of this needs a database. Where none can be reached nothing is
//! measured, which is not evidence that anything is indexed.

use super::fixture;
use super::harness;
use super::statistics::{analyze, index_reads, publish_statistics, sequential_rows_read};

use nitinol_persistence::store::Journal;
use nitinol_sqlx::PostgresEventStore;

/// The index the head is answered from.
const INDEX: &str = "nitinol_journal_global_sequence_key";

/// How many rows the journal is seeded with.
const ROWS: u64 = 5_000;

#[tokio::test]
async fn a_journal_nothing_was_appended_to_has_no_head() {
    let Some(backend) = harness::postgres_backend().await else {
        return;
    };
    let store = backend.bring_up_store().await;

    let head = store
        .head()
        .await
        .unwrap_or_else(|failure| panic!("a migrated schema answers its head: {failure}"));

    assert_eq!(
        head, None,
        "a journal holding no rows has no head; a position of zero would read as a journal that \
         ends at an event numbered 0, which no store assigns"
    );
}

#[tokio::test]
async fn the_head_is_read_off_the_global_sequence_index() {
    let Some(backend) = harness::postgres_backend().await else {
        return;
    };
    let schema = backend.migrated_schema().await;
    // One connection for the store and for the counters alike, for the reason
    // `MigratedSchema::pinned_pool` gives.
    let pool = schema.pinned_pool().await;
    let store = Journal::from(
        PostgresEventStore::new(pool.clone())
            .expect("the event types this test binary linked claim distinct canonical paths"),
    );

    let last_assigned = seed(&store).await;
    analyze(&pool).await;

    publish_statistics(&pool).await;
    let before = index_reads(&pool, INDEX).await;
    let sequential_before = sequential_rows_read(&pool).await;

    let head = store
        .head()
        .await
        .unwrap_or_else(|failure| panic!("a seeded schema answers its head: {failure}"));

    // The measurement is only about an index if the answer it measures was
    // correct, so the answer is checked first.
    assert_eq!(
        head,
        Some(last_assigned),
        "the head is the highest global_sequence the store assigned, and the last append of the \
         seeding was assigned up to {last_assigned}"
    );

    publish_statistics(&pool).await;
    let after = index_reads(&pool, INDEX).await;
    let sequential_after = sequential_rows_read(&pool).await;

    let scans = after.scans - before.scans;
    let entries = after.entries - before.entries;
    assert!(
        scans > 0,
        "the head the provider asked for must be served by {INDEX}: it is the highest \
         global_sequence, which is the end that index is ordered by. No scan of it was counted, \
         so the provider's own statement cannot use it"
    );
    assert!(
        entries <= 4,
        "answering the head took {entries} entries off {INDEX}. The highest position is the one \
         entry at the end of the index, and reading more of it than that is reading the journal \
         to find where it ends"
    );
    // A bound rather than an equality: housekeeping the database starts on its
    // own can read the table while this test is between two readings, and that
    // is not the statement under measurement. What is refused is a head that
    // read the journal through.
    let sequential = sequential_after - sequential_before;
    let rows = i64::try_from(ROWS).expect("the seeded row count fits in i64");
    assert!(
        sequential < rows / 4,
        "answering the head read {sequential} row(s) sequentially out of {rows} seeded; finding \
         where the journal ends must not come to reading it through"
    );
}

/// Seed the journal across two streams, and answer the last position the store
/// assigned.
async fn seed(store: &Journal) -> u64 {
    const FIRST: &str = "head.probe.stream-a";
    const SECOND: &str = "head.probe.stream-b";
    const BATCH: u64 = 500;

    let mut last_assigned = None;
    let mut sequences = [0u64, 0u64];
    let mut seeded = 0u64;
    let mut batches = 0usize;
    while seeded < ROWS {
        let slot = batches % 2;
        let stream = [FIRST, SECOND][slot];
        let batch = (0..BATCH)
            .map(|_| {
                sequences[slot] += 1;
                // The payload is the same for every row: what is measured is
                // which rows the store had to read, not what they carried.
                fixture::recorded(sequences[slot], b"{}")
            })
            .collect();
        last_assigned = Some(fixture::highest(
            fixture::committed(store, stream, batch).await,
        ));
        seeded += BATCH;
        batches += 1;
    }
    last_assigned.expect("the seeding appends at least one batch")
}
