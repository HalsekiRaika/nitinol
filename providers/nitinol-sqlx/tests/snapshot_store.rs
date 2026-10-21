//! What the Postgres `SnapshotStore` owes a caller, on its own.
//!
//! ```gherkin
//! Feature: nitinol-sqlx SnapshotStore (Postgres)
//!
//!   Scenario: a saved snapshot reads back
//!     Given a migrated Postgres and a PostgresSnapshotStore over it
//!     When an aggregate's snapshot is saved at sequence 5
//!     Then load_latest answers with that payload and sequence 5
//!
//!   Scenario: the save that came last wins
//!     Given a snapshot saved at sequence 5
//!     When the same aggregate is saved at sequence 9
//!     Then load_latest answers with sequence 9
//!
//!   Scenario: an aggregate nothing has saved for
//!     Given an aggregate that has never been snapshotted
//!     When load_latest is called
//!     Then it answers Ok(None) rather than failing
//!
//!   Scenario: one aggregate's snapshot is never another's
//!     Given only aggregate A's snapshot is saved
//!     When aggregate B is loaded
//!     Then it answers Ok(None)
//! ```
//!
//! The suite the conformance crate publishes runs `SNAP-1` … `SNAP-4` too,
//! through the slot the assembled store answers with — `provider_conformance.rs`
//! is where that happens. What is fixed here is this backend's own answer to
//! the one clause the contract leaves to a backend, and the two readings the
//! suite does not make: the instant a caller handed over, and a row the store
//! cannot read.
//!
//! # A save behind the stored position
//!
//! The contract leaves a save at a position the stored snapshot has already
//! reached, or behind it, to the backend: a single writer takes snapshots at
//! positions that only grow, so taking such a save and refusing it are both
//! consistent with every consumer there is. This backend keeps whichever save
//! came last, which is the answer the in-memory reference store gives, and that
//! is what the clause below reads. An upsert carrying a condition on the
//! sequence would answer differently, and a deployment moving between the two
//! stores would find its snapshots behaving differently for it.
//!
//! Running any of this needs a database. Where none can be reached the clause
//! judges nothing, which is not evidence that it holds.

#[path = "common/fixture.rs"]
mod fixture;
#[path = "common/harness.rs"]
mod harness;

use bytes::Bytes;
use jiff::Timestamp;
use nitinol_persistence::error::SnapshotError;
use nitinol_persistence::store::SnapshotStore;
use nitinol_persistence::{AggregateId, PersistedSnapshot};
use nitinol_sqlx::PostgresSnapshotStore;

/// Every observable field of a snapshot the store served back.
///
/// `PersistedSnapshot` does not derive `PartialEq`, and the fields one save
/// wrote are judged **together**: a store answering with the latest position
/// beside an earlier state would pass a reading that checked the position
/// alone, and that pairing is a state no save ever wrote.
#[derive(Debug, PartialEq)]
struct Served {
    aggregate_id: AggregateId,
    sequence: u64,
    payload: Bytes,
    created_at: Timestamp,
}

impl From<PersistedSnapshot> for Served {
    fn from(snapshot: PersistedSnapshot) -> Self {
        Self {
            aggregate_id: snapshot.aggregate_id,
            sequence: snapshot.sequence,
            payload: snapshot.payload,
            created_at: snapshot.created_at,
        }
    }
}

/// What a clause requires the store to serve for an aggregate it saved.
fn served(aggregate_id: &AggregateId, sequence: u64, payload: &'static [u8]) -> Option<Served> {
    Some(Served {
        aggregate_id: aggregate_id.clone(),
        sequence,
        payload: Bytes::from_static(payload),
        created_at: fixture::fixed_instant(),
    })
}

/// A snapshot of `aggregate_id` at `sequence`, carrying `payload` as the state
/// it had there.
///
/// The instant is the fixture's fixed one rather than the clock's, so what a
/// round trip is compared against is the value the caller supplied and two runs
/// compare the same thing.
fn snapshot(
    aggregate_id: &AggregateId,
    sequence: u64,
    payload: &'static [u8],
) -> PersistedSnapshot {
    taken_at(aggregate_id, sequence, payload, fixture::fixed_instant())
}

fn taken_at(
    aggregate_id: &AggregateId,
    sequence: u64,
    payload: &'static [u8],
    created_at: Timestamp,
) -> PersistedSnapshot {
    PersistedSnapshot {
        aggregate_id: aggregate_id.clone(),
        sequence,
        payload: Bytes::from_static(payload),
        created_at,
    }
}

/// A save a clause requires to be taken.
async fn saved(store: &PostgresSnapshotStore, snapshot: PersistedSnapshot) {
    let (aggregate_id, sequence) = (snapshot.aggregate_id.clone(), snapshot.sequence);
    store.save(snapshot).await.unwrap_or_else(|failure| {
        panic!(
            "saving a snapshot of {:?} at sequence {sequence} was refused with {failure}",
            aggregate_id.as_str()
        )
    });
}

/// What the store serves for `aggregate_id`.
async fn latest(store: &PostgresSnapshotStore, aggregate_id: &AggregateId) -> Option<Served> {
    store
        .load_latest(aggregate_id)
        .await
        .expect("a load against a database this run reached must answer")
        .map(Served::from)
}

/// Acceptance: "an aggregate nothing has saved for" — `SNAP-1`.
#[tokio::test]
async fn an_aggregate_nothing_has_saved_for_has_no_snapshot() {
    let Some(backend) = harness::postgres_backend().await else {
        return;
    };
    let schema = backend.migrated_schema().await;
    let store = PostgresSnapshotStore::new(schema.pool());
    let unsaved = AggregateId::new("sqlx.snapshot.unsaved");

    assert_eq!(
        latest(&store, &unsaved).await,
        None,
        "an aggregate the store holds no snapshot for is answered rather than failed: every \
         aggregate activating for the first time is in that state, and it carries on from its \
         default state and replays from its first event"
    );
}

/// Acceptance: "a saved snapshot reads back".
#[tokio::test]
async fn a_saved_snapshot_reads_back_with_the_position_it_was_taken_at() {
    let Some(backend) = harness::postgres_backend().await else {
        return;
    };
    let schema = backend.migrated_schema().await;
    let store = PostgresSnapshotStore::new(schema.pool());
    let snapshotted = AggregateId::new("sqlx.snapshot.round-trip");

    saved(&store, snapshot(&snapshotted, 5, b"at-five")).await;

    assert_eq!(
        latest(&store, &snapshotted).await,
        served(&snapshotted, 5, b"at-five"),
        "what one save wrote is what a load reads back, and the aggregate it was for, the \
         position it was taken at and the state at that position are one save's together"
    );
}

/// Acceptance: "the save that came last wins" — `SNAP-3`, at ascending
/// positions.
#[tokio::test]
async fn the_save_that_came_last_is_what_a_load_answers_with() {
    let Some(backend) = harness::postgres_backend().await else {
        return;
    };
    let schema = backend.migrated_schema().await;
    let store = PostgresSnapshotStore::new(schema.pool());
    let snapshotted = AggregateId::new("sqlx.snapshot.ascending");

    saved(&store, snapshot(&snapshotted, 5, b"at-five")).await;
    saved(&store, snapshot(&snapshotted, 9, b"at-nine")).await;

    assert_eq!(
        latest(&store, &snapshotted).await,
        served(&snapshotted, 9, b"at-nine"),
        "the save that came last is what a load answers with, and it answers with that save's \
         position and that save's state together — the position from 9 beside the state from 5 \
         would lose every event between them with nothing said"
    );
}

/// This backend's own answer to the clause the contract leaves open: a save at
/// a position behind the stored one replaces it, the way the reference store's
/// does.
///
/// An upsert carrying `WHERE stored.sequence < incoming.sequence`, or one doing
/// nothing on a conflict, would keep the snapshot at 9 here and pass every
/// clause the suite runs.
#[tokio::test]
async fn a_save_behind_the_stored_position_replaces_it_too() {
    let Some(backend) = harness::postgres_backend().await else {
        return;
    };
    let schema = backend.migrated_schema().await;
    let store = PostgresSnapshotStore::new(schema.pool());
    let snapshotted = AggregateId::new("sqlx.snapshot.descending");

    saved(&store, snapshot(&snapshotted, 9, b"at-nine")).await;
    saved(&store, snapshot(&snapshotted, 5, b"at-five")).await;

    assert_eq!(
        latest(&store, &snapshotted).await,
        served(&snapshotted, 5, b"at-five"),
        "a save behind the stored position is taken rather than refused or absorbed, which is \
         the answer the in-memory reference store gives; a load must then answer with it, state \
         and position together"
    );
}

/// Acceptance: "one aggregate's snapshot is never another's" — `SNAP-4`.
#[tokio::test]
async fn one_aggregates_snapshot_is_never_anothers() {
    let Some(backend) = harness::postgres_backend().await else {
        return;
    };
    let schema = backend.migrated_schema().await;
    let store = PostgresSnapshotStore::new(schema.pool());
    let snapshotted = AggregateId::new("sqlx.snapshot.one");
    let other = AggregateId::new("sqlx.snapshot.other");

    saved(&store, snapshot(&snapshotted, 5, b"at-five")).await;

    assert_eq!(
        latest(&store, &other).await,
        None,
        "a save for one aggregate changes what a load answers for that aggregate and for no \
         other: an aggregate the store holds nothing for is still answered with none of its own \
         while the store holds another's"
    );

    saved(&store, snapshot(&other, 2, b"at-two")).await;

    assert_eq!(
        latest(&store, &snapshotted).await,
        served(&snapshotted, 5, b"at-five"),
        "once both aggregates have a snapshot each, a load answers with its own rather than with \
         whichever was written last"
    );
}

/// The instant is stored as the caller handed it over, the way the journal
/// keeps an event's own.
///
/// Nothing in the framework reads it, so what is read here is only that the
/// column holds the caller's value: a column defaulting to the database's clock
/// with the saved instant thrown away would answer with when the row was
/// written instead of when the snapshot was taken.
#[tokio::test]
async fn the_instant_a_snapshot_was_taken_at_comes_back_as_it_went_in() {
    let Some(backend) = harness::postgres_backend().await else {
        return;
    };
    let schema = backend.migrated_schema().await;
    let store = PostgresSnapshotStore::new(schema.pool());
    let snapshotted = AggregateId::new("sqlx.snapshot.taken-at");
    let long_before_this_run = Timestamp::from_microsecond(1_000_000_000_654_321)
        .expect("a microsecond count inside the supported range is a timestamp");

    saved(
        &store,
        taken_at(&snapshotted, 5, b"at-five", long_before_this_run),
    )
    .await;

    assert_eq!(
        latest(&store, &snapshotted)
            .await
            .map(|answer| answer.created_at),
        Some(long_before_this_run),
        "the instant a snapshot carries is the one the caller supplied, so a load hands back \
         when the snapshot was taken rather than when its row was written"
    );
}

/// A clone of the handle reaches the same rows.
///
/// The store is distributed by being cloned — that is the whole of what the
/// contract asks an implementor to write — so a clone reaching rows of its own
/// would leave an aggregate replaying from its first event beside a snapshot
/// somebody saved.
#[tokio::test]
async fn a_clone_of_the_store_reaches_the_same_rows() {
    let Some(backend) = harness::postgres_backend().await else {
        return;
    };
    let schema = backend.migrated_schema().await;
    let store = PostgresSnapshotStore::new(schema.pool());
    let snapshotted = AggregateId::new("sqlx.snapshot.cloned");

    saved(&store, snapshot(&snapshotted, 5, b"at-five")).await;
    let cloned = store.clone();

    assert_eq!(
        latest(&cloned, &snapshotted).await,
        served(&snapshotted, 5, b"at-five"),
        "a snapshot saved through one handle is read through its clone: the clone copies the \
         pool handle, so the two reach the same rows"
    );
}

/// A row the store cannot read is reported rather than passed over.
///
/// The column reaches further than an instant does, so a row can hold a moment
/// this store cannot serve. Answering with no snapshot for it could not be told
/// from an aggregate that has never been snapshotted, and an activation would
/// then replay from its first event and overwrite the snapshot it could not
/// read, with nothing said anywhere.
#[tokio::test]
async fn a_row_holding_an_instant_the_store_cannot_serve_is_reported() {
    let Some(backend) = harness::postgres_backend().await else {
        return;
    };
    let schema = backend.migrated_schema().await;
    let pool = schema.pool();
    let store = PostgresSnapshotStore::new(pool.clone());
    let snapshotted = AggregateId::new("sqlx.snapshot.past-every-instant");

    sqlx::query(
        r#"
        INSERT INTO nitinol_snapshot (aggregate_id, sequence, payload, created_at)
        VALUES ($1, 5, '\x00'::bytea, '10000-01-01 00:00:00+00'::timestamptz)
        "#,
    )
    .bind(snapshotted.as_str())
    .execute(&pool)
    .await
    .expect("the schema this clause was handed must take a row written by hand");

    match store.load_latest(&snapshotted).await {
        Err(SnapshotError::Backend(_)) => {}
        Ok(answer) => panic!(
            "a stored instant past every instant this store can serve must reach the caller as \
             a backend failure — never clamped, and never passed over as an aggregate with no \
             snapshot; the store answered {answer:?}"
        ),
    }
}
