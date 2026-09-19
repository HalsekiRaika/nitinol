//! What a column has to give back unchanged: the instant an event occurred at,
//! and the type it carries.
//!
//! ```gherkin
//!   Scenario: occurred_at survives the column
//!     Given events carrying instants of the caller's choosing
//!     When they are read back
//!     Then each one carries the instant it was appended with, held at the
//!          resolution the column has — never the moment the row was written
//!
//!   Scenario: 手動登録なしで round-trip できる
//!     Given events of every shape an event type has, declared to the
//!          collection and nowhere else
//!     When a store opened with `PostgresEventStore::new(pool)` appends and
//!          loads them
//!     Then each one is restored as the event type it was stored from
//!
//!   Scenario: an event type nothing collected
//!     Then the row fails its load rather than being passed over
//! ```
//!
//! Collection is what makes the second scenario possible at all: every part of
//! an `EventType` is a `&'static str`, so a value read out of a column cannot
//! become one without leaking the string it was read into. What the store
//! serves is what its binary linked, and the store takes no registry from its
//! caller — which is what the third scenario turns on, since an event type this
//! file declares no entry for is appended all the same (an append writes the
//! path it is handed and asks the collection nothing).
//!
//! Running any of this needs a database. Where none can be reached the clause
//! judges nothing, which is not evidence that it holds.

#[path = "common/fixture.rs"]
mod fixture;
#[path = "common/harness.rs"]
mod harness;

use fixture::{at, committed, observe, typed, OCCURRED_AT_MICROSECOND};
use futures_util::StreamExt;
use jiff::Timestamp;
use linkme::distributed_slice;
use nitinol_persistence::error::LoadError;
use nitinol_persistence::{
    EventRegistryEntry, EventType, Family, LoadQuery, TypeName, EVENT_REGISTRY,
};
use nitinol_sqlx::UnknownEventType;

const BARE: EventType = EventType::new(Family::new(""), TypeName::new("Bare"));
const NESTED: EventType =
    EventType::new(Family::new("sqlx.round-trip.deep"), TypeName::new("Nested"));
const CHOICE: EventType = EventType::new(Family::new("sqlx.round-trip"), TypeName::new("Choice"));

/// The three shapes a canonical path has — no family, a deep family, a shallow
/// one — each declared the way a `#[derive(Event)]` type declares itself: one
/// entry per declaring type, carrying the one path it is written down as.
#[distributed_slice(EVENT_REGISTRY)]
static BARE_ENTRY: EventRegistryEntry =
    EventRegistryEntry::new(concat!(module_path!(), "::Bare"), &[BARE]);

#[distributed_slice(EVENT_REGISTRY)]
static NESTED_ENTRY: EventRegistryEntry =
    EventRegistryEntry::new(concat!(module_path!(), "::Nested"), &[NESTED]);

#[distributed_slice(EVENT_REGISTRY)]
static CHOICE_ENTRY: EventRegistryEntry =
    EventRegistryEntry::new(concat!(module_path!(), "::Choice"), &[CHOICE]);

/// An event type with no entry anywhere in this binary.
///
/// Appending it is legal — an append writes the canonical path it is handed —
/// and loading it is what nothing can answer for.
const UNCOLLECTED: EventType =
    EventType::new(Family::new("sqlx.round-trip"), TypeName::new("Uncollected"));

#[tokio::test]
async fn occurred_at_round_trips_at_the_microsecond_resolution_the_column_holds() {
    const STREAM: &str = "sqlx.round-trip.occurred-at";

    let Some(backend) = harness::postgres_backend().await else {
        return;
    };
    let store = backend.bring_up_store().await;

    let aligned = microseconds(OCCURRED_AT_MICROSECOND);
    let sub_microsecond = nanoseconds(i128::from(OCCURRED_AT_MICROSECOND) * 1_000 + 789);
    let before_epoch = nanoseconds(-1_234_567_891);

    committed(
        &store,
        STREAM,
        vec![
            at(1, b"aligned", aligned),
            at(2, b"sub-microsecond", sub_microsecond),
            at(3, b"before-epoch", before_epoch),
        ],
    )
    .await;

    let served = observe(&store, LoadQuery::by_stream(STREAM)).await;

    let round_tripped: Vec<Timestamp> = served.iter().map(|event| event.occurred_at).collect();
    assert_eq!(
        round_tripped,
        vec![
            aligned,
            microseconds(OCCURRED_AT_MICROSECOND),
            microseconds(-1_234_567),
        ],
        "occurred_at is the caller's instant at the column's microsecond resolution, a finer \
         component truncated towards zero — never the moment the row was written"
    );
}

#[tokio::test]
async fn every_shape_of_event_type_round_trips_with_nothing_registered_by_hand() {
    const STREAM: &str = "sqlx.round-trip.event-type";

    let Some(backend) = harness::postgres_backend().await else {
        return;
    };
    let store = backend.bring_up_store().await;
    committed(
        &store,
        STREAM,
        vec![
            typed(1, BARE, b"bare"),
            typed(2, NESTED, b"nested"),
            typed(3, CHOICE, b"choice"),
        ],
    )
    .await;

    let served = observe(&store, LoadQuery::by_stream(STREAM)).await;

    let round_tripped: Vec<EventType> = served.iter().map(|event| event.event_type).collect();
    assert_eq!(
        round_tripped,
        vec![BARE, NESTED, CHOICE],
        "a store opened over a pool alone restores every event type its binary declared — an \
         empty family and a deep one included — and the declaration was a linked entry, not a \
         registry handed to the store"
    );
}

#[tokio::test]
async fn a_row_carrying_an_uncollected_event_type_fails_its_load_instead_of_vanishing() {
    const STREAM: &str = "sqlx.round-trip.uncollected";

    let Some(backend) = harness::postgres_backend().await else {
        return;
    };
    let store = backend.bring_up_store().await;
    committed(&store, STREAM, vec![typed(1, UNCOLLECTED, b"e1")]).await;

    let mut rows = store
        .load(LoadQuery::by_stream(STREAM))
        .await
        .expect("a load a store can answer at all opens its stream");

    let first = rows.next().await.expect(
        "the row is served rather than passed over: a reader that never sees it never learns it \
         is there",
    );
    match first {
        Err(LoadError::Backend(failure)) => {
            let Some(unknown) = failure.downcast_ref::<UnknownEventType>() else {
                panic!(
                    "a row whose event type nothing collected fails with the path it carried, and \
                     the store reported {failure}"
                )
            };
            assert_eq!(
                unknown.path,
                UNCOLLECTED.to_path().to_string(),
                "the failure names the canonical path the row carried — which is how a link that \
                 dropped a crate's declarations is told apart from a corrupt row"
            );
        }
        Ok(event) => panic!(
            "a row whose event type nothing collected cannot be restored, and the store served it \
             as {:?}",
            event.event_type
        ),
    }
    assert!(
        rows.next().await.is_none(),
        "the stream held one row, and the store served more than one"
    );
}

fn microseconds(count: i64) -> Timestamp {
    Timestamp::from_microsecond(count)
        .expect("a microsecond count inside the supported range is a timestamp")
}

fn nanoseconds(count: i128) -> Timestamp {
    Timestamp::from_nanosecond(count)
        .expect("a nanosecond count inside the supported range is a timestamp")
}
