//! What a load query means when it names more than one condition.
//!
//! `LOAD-1`: the conditions of a [`LoadQuery`] are a conjunction. A query
//! naming an event-type prefix and a global position returns the rows that
//! satisfy both, in ascending `global_sequence` order, and nothing else.
//!
//! This is the reading a checkpoint-following consumer of one event type rests
//! on: it asks for its own type above the position it has finished with, and
//! anything else coming back — a row of another type, or a row at or below the
//! position — would either be delivered to a consumer that cannot read it or
//! be delivered twice.
//!
//! These tests run the clause against [`InMemoryEventStore`] as the reference
//! implementation. What binds a backend is the clause on
//! [`EventStore::load`]; what a backend of its own is put through is
//! `nitinol-store-conformance`, which runs the same clause against any store.

use bytes::Bytes;
use futures_util::TryStreamExt;
use jiff::Timestamp;

use nitinol_persistence::store::{EventStore, InMemoryEventStore};
use nitinol_persistence::{
    AppendingEvent, EventType, Family, LoadQuery, LoadedEvent, MaterializedPath, TypeName,
};

const FAMILY: Family = Family::new("conj");
const SELECTED: EventType = EventType::new(FAMILY, TypeName::new("Selected"));
const OTHER: EventType = EventType::new(FAMILY, TypeName::new("Other"));

fn row(sequence: u64, event_type: EventType) -> AppendingEvent {
    AppendingEvent {
        sequence,
        event_type,
        payload: Bytes::from_static(b"{}"),
        occurred_at: Timestamp::now(),
    }
}

/// A journal in which the two event types alternate across two streams, so no
/// condition can be satisfied by accident: neither type occupies a contiguous
/// stretch of the global order, and neither owns a stream.
async fn seeded() -> (InMemoryEventStore, Vec<u64>) {
    let store = InMemoryEventStore::default();
    let mut positions = Vec::new();
    for (nth, (stream, event_type)) in [
        ("stream-a", SELECTED),
        ("stream-b", OTHER),
        ("stream-a", SELECTED),
        ("stream-b", OTHER),
        ("stream-a", SELECTED),
    ]
    .into_iter()
    .enumerate()
    {
        let sequence = u64::try_from(nth).expect("a fixture index fits in u64") + 1;
        let outcome = store
            .append(stream, vec![row(sequence, event_type)])
            .await
            .expect("appending a fixture row succeeds");
        positions.push(
            *outcome
                .assigned_sequences
                .first()
                .expect("a one-event append is assigned one global position"),
        );
    }
    (store, positions)
}

async fn loaded(store: &InMemoryEventStore, query: LoadQuery) -> Vec<LoadedEvent> {
    store
        .load(query)
        .await
        .expect("loading from the reference implementation succeeds")
        .try_collect()
        .await
        .expect("collecting a loaded stream succeeds")
}

fn unmatched_prefix() -> MaterializedPath {
    "conj.Absent"
        .parse()
        .expect("a fixture prefix parses as a materialized path")
}

#[tokio::test]
async fn load_1_a_prefix_and_a_position_are_both_required() {
    let (store, positions) = seeded().await;
    // Above the first two rows, so one row of each type is below the floor.
    let floor = positions[2];

    let rows = loaded(
        &store,
        LoadQuery {
            event_type_prefix: Some(SELECTED.to_path()),
            from_global_sequence: Some(floor),
            ..Default::default()
        },
    )
    .await;

    assert_eq!(
        rows.iter()
            .map(|row| (row.global_sequence, row.event_type))
            .collect::<Vec<_>>(),
        vec![(positions[2], SELECTED), (positions[4], SELECTED)],
        "the rows returned are the ones of the named type at or above the named \
         position, in ascending global order: rows of another type above the \
         position are left out, and so are rows of the named type below it"
    );
}

#[tokio::test]
async fn load_1_a_prefix_matching_nothing_returns_nothing_whatever_the_position() {
    let (store, positions) = seeded().await;

    for floor in [1, positions[2], positions[4]] {
        let rows = loaded(
            &store,
            LoadQuery {
                event_type_prefix: Some(unmatched_prefix()),
                from_global_sequence: Some(floor),
                ..Default::default()
            },
        )
        .await;

        assert!(
            rows.is_empty(),
            "with a prefix no stored row is within, the position condition \
             cannot admit anything: the two conditions are a conjunction, not \
             an alternative. Floor {floor} returned {} rows",
            rows.len()
        );
    }
}

#[tokio::test]
async fn load_1_an_ancestor_prefix_admits_every_descendant_above_the_position() {
    let (store, positions) = seeded().await;
    let floor = positions[2];

    let rows = loaded(
        &store,
        LoadQuery {
            event_type_prefix: Some(
                "conj"
                    .parse()
                    .expect("a fixture prefix parses as a materialized path"),
            ),
            from_global_sequence: Some(floor),
            ..Default::default()
        },
    )
    .await;

    assert_eq!(
        rows.iter()
            .map(|row| (row.global_sequence, row.event_type))
            .collect::<Vec<_>>(),
        vec![
            (positions[2], SELECTED),
            (positions[3], OTHER),
            (positions[4], SELECTED),
        ],
        "a prefix naming an ancestor admits both of its descendants, and the \
         position condition still applies to all of them"
    );
}

#[tokio::test]
async fn load_1_the_position_is_inclusive_of_the_row_it_names() {
    let (store, positions) = seeded().await;

    let rows = loaded(
        &store,
        LoadQuery {
            event_type_prefix: Some(SELECTED.to_path()),
            from_global_sequence: Some(positions[4]),
            ..Default::default()
        },
    )
    .await;

    assert_eq!(
        rows.iter()
            .map(|row| row.global_sequence)
            .collect::<Vec<_>>(),
        vec![positions[4]],
        "the named position is the first one admitted, which is what lets a \
         consumer ask for `everything from here` by naming the position after \
         the one it finished with"
    );
}
