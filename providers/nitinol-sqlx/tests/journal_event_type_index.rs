//! What a load for one event type above a position actually reads.
//!
//! ```gherkin
//!   Scenario: a consumer of one event type polls a journal it is sparse in
//!     Given a journal where another event type outnumbers the one asked for
//!     When a load asks for that type above a position
//!     Then the store reads about as many rows as it serves
//!     And it does not read the whole journal above the position
//! ```
//!
//! A consumer following one event type across every stream is the shape a saga
//! delivery host has: its own decisions are rare on its own streams, because
//! the framework writes its records there too. Served by the global-order index
//! alone, such a load reads every row above the position and discards most of
//! them — and a stretch where its type does not appear leaves the position
//! where it was, so the same rows are read again on the next poll.
//!
//! What is measured here is the load the provider itself issues, rather than a
//! copy of its statement: the database's own counters say which index was
//! scanned and how many entries came off it. A test that reassembled the SQL
//! would measure the copy, and would keep passing if the provider's own
//! statement stopped being able to use the index.
//!
//! Running any of this needs a database. Where none can be reached nothing is
//! measured, which is not evidence that anything is indexed.

#[path = "common/fixture.rs"]
mod fixture;
#[path = "common/harness.rs"]
mod harness;

use std::sync::Arc;

use linkme::distributed_slice;
use nitinol_persistence::store::EventStore;
use nitinol_persistence::{
    AppendingEvent, EventRegistryEntry, EventType, Family, LoadQuery, TypeName, EVENT_REGISTRY,
};
use nitinol_sqlx::PostgresEventStore;
use sqlx::PgPool;

/// The type the consumer follows — rare on the streams it is stored in.
const FOLLOWED: EventType = EventType::new(Family::new("index.probe"), TypeName::new("Followed"));

/// The type that outnumbers it, standing for the records a framework writes
/// onto the same streams.
const CROWD: EventType = EventType::new(Family::new("index.probe"), TypeName::new("Crowd"));

#[distributed_slice(EVENT_REGISTRY)]
static INDEX_PROBE_TYPES: EventRegistryEntry = EventRegistryEntry::new(
    concat!(module_path!(), "::index_probe_types"),
    &[FOLLOWED, CROWD],
);

/// The index this clause is about.
const INDEX: &str = "nitinol_journal_event_type_global_sequence";

/// How many rows the journal is seeded with.
const ROWS: u64 = 30_000;

/// One row in this many carries [`FOLLOWED`].
const FOLLOWED_EVERY: u64 = 200;

/// Where the consumer resumes from — far enough in that the rows above it are
/// many, and that reading all of them would be plainly different from reading
/// the ones served.
const FLOOR: u64 = 12_000;

/// What the database counted against one index.
struct IndexReads {
    /// How many times a scan of this index was started.
    scans: i64,
    /// How many index entries those scans returned.
    entries: i64,
}

#[tokio::test]
async fn a_load_for_one_event_type_reads_that_type_rather_than_the_journal() {
    let Some(backend) = harness::postgres_backend().await else {
        return;
    };
    let schema = backend.migrated_schema().await;
    // One connection for the store and for the counters alike: the load is only
    // measurable where the backend that ran it is the backend that publishes
    // what it did.
    let pool = schema.pinned_pool().await;
    let store: Arc<dyn EventStore> = Arc::new(
        PostgresEventStore::new(pool.clone())
            .expect("the event types this test binary linked claim distinct canonical paths"),
    );

    let followed = seed(&store).await;
    analyze(&pool).await;

    publish_statistics(&pool).await;
    let before = index_reads(&pool).await;
    let sequential_before = sequential_rows_read(&pool).await;

    let served = fixture::observe(
        &store,
        LoadQuery {
            event_type_prefix: Some(FOLLOWED.to_path()),
            from_global_sequence: Some(FLOOR),
            ..LoadQuery::default()
        },
    )
    .await;

    // The measurement is only about an index if the read it measures was
    // correct, so what came back is checked first.
    let positions: Vec<u64> = served.iter().map(|event| event.global_sequence).collect();
    let expected: Vec<u64> = followed.iter().copied().filter(|at| *at >= FLOOR).collect();
    assert_eq!(
        positions, expected,
        "the load must serve the followed type's rows at or above {FLOOR}, in ascending global \
         order, before anything is concluded from how they were read"
    );
    assert!(
        !expected.is_empty() && expected.len() < 1_000,
        "the seeding must leave the followed type sparse above the position for the counts below \
         to mean anything; {} of {ROWS} rows were served",
        expected.len()
    );

    publish_statistics(&pool).await;
    let after = index_reads(&pool).await;
    let sequential_after = sequential_rows_read(&pool).await;

    let scans = after.scans - before.scans;
    let entries = after.entries - before.entries;
    let served_count = i64::try_from(served.len()).expect("a served count fits in i64");

    assert!(
        scans > 0,
        "the load the provider issued must be served by {INDEX}: it asked for one event type \
         above a position, which is the pair that index is ordered by. No scan of it was counted, \
         so the provider's own statement cannot use it — an index nothing reaches is not an index"
    );
    assert!(
        entries <= served_count * 4,
        "a load serving {served_count} row(s) took {entries} entries off {INDEX}. What the store \
         reads has to be of the order of what it serves: reading the whole journal above the \
         position and discarding the rest is the cost this index exists to remove, and {ROWS} \
         rows were seeded with the followed type one in {FOLLOWED_EVERY}"
    );
    // A bound rather than an equality: housekeeping the database starts on its
    // own can read the table while this test is between two readings, and that
    // is not the load under measurement. What is refused is a load that read
    // the journal through.
    let sequential = sequential_after - sequential_before;
    let rows = i64::try_from(ROWS).expect("the seeded row count fits in i64");
    assert!(
        sequential < rows / 4,
        "the load read {sequential} row(s) sequentially out of {rows} seeded; serving one event \
         type above a position must not come to reading the journal through"
    );
}

/// Seed a journal the followed type is sparse in, and answer the positions it
/// was stored at.
///
/// Two streams, so neither type owns one and the condition cannot be satisfied
/// by the stream key instead.
async fn seed(store: &Arc<dyn EventStore>) -> Vec<u64> {
    const FIRST: &str = "index.probe.stream-a";
    const SECOND: &str = "index.probe.stream-b";
    const BATCH: u64 = 500;

    let mut followed = Vec::new();
    let mut sequence = 0u64;
    let mut batches = 0u64;
    while sequence < ROWS {
        let mut batch = Vec::new();
        let mut carries_followed = Vec::new();
        for _ in 0..BATCH {
            sequence += 1;
            let is_followed = sequence.is_multiple_of(FOLLOWED_EVERY);
            carries_followed.push(is_followed);
            batch.push(row(sequence, if is_followed { FOLLOWED } else { CROWD }));
        }
        let stream = if batches.is_multiple_of(2) {
            FIRST
        } else {
            SECOND
        };
        batches += 1;
        let outcome = fixture::committed(store, stream, batch).await;
        // One assigned position per event, in the order the batch carried them.
        for (position, is_followed) in outcome.assigned_sequences.iter().zip(carries_followed) {
            if is_followed {
                followed.push(*position);
            }
        }
    }
    followed.sort_unstable();
    followed
}

fn row(sequence: u64, event_type: EventType) -> AppendingEvent {
    // The payload is the same for every row: what is measured is which rows the
    // store had to read, not what they carried.
    fixture::typed(sequence, event_type, b"{}")
}

/// Let the planner see what was seeded.
///
/// Without it the planner is choosing against the estimates of an empty table,
/// so what a plan did would say nothing about the index.
async fn analyze(pool: &PgPool) {
    sqlx::query("ANALYZE nitinol_journal")
        .execute(pool)
        .await
        .expect("a migrated schema can be analyzed");
}

/// Publish what this connection has done so far to the shared counters.
///
/// A backend holds its own tally and reports it when a transaction of its own
/// ends, and not more often than once a second unless asked. Asking is what
/// makes a reading taken straight after a load a reading of that load.
async fn publish_statistics(pool: &PgPool) {
    sqlx::query("SELECT pg_stat_force_next_flush()")
        .execute(pool)
        .await
        .expect("a database this run reached publishes its own activity counters");
}

async fn index_reads(pool: &PgPool) -> IndexReads {
    let row: Option<(i64, i64)> = sqlx::query_as(
        r#"
        SELECT coalesce(idx_scan, 0), coalesce(idx_tup_read, 0)
          FROM pg_stat_all_indexes
         WHERE schemaname = current_schema() AND indexrelname = $1
        "#,
    )
    .bind(INDEX)
    .fetch_optional(pool)
    .await
    .expect("the statistics views answer on a database this run reached");

    let (scans, entries) = row.unwrap_or_else(|| {
        panic!(
            "the provider's migrations must create {INDEX}: without it a consumer of one event \
             type is served by the global-order index alone"
        )
    });
    IndexReads { scans, entries }
}

/// How many rows the journal has been read sequentially.
async fn sequential_rows_read(pool: &PgPool) -> i64 {
    sqlx::query_scalar(
        r#"
        SELECT coalesce(seq_tup_read, 0)
          FROM pg_stat_all_tables
         WHERE schemaname = current_schema() AND relname = 'nitinol_journal'
        "#,
    )
    .fetch_one(pool)
    .await
    .expect("the statistics views answer for a table this run migrated")
}
