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
//!   Scenario: an event type is restored through the registry
//!     Given events of every shape an event type has
//!     Then each one is restored as the registered type it was stored from
//!
//!   Scenario: an event type nobody registered
//!     Then the row fails its load rather than being passed over
//! ```
//!
//! The registry is what makes the second scenario possible at all: every part
//! of an `EventType` is a `&'static str`, so a value read out of a column
//! cannot become one without leaking the string it was read into.
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
use nitinol_persistence::error::LoadError;
use nitinol_persistence::{EventType, Family, LoadQuery, TypeName, Variant};
use nitinol_sqlx::UnknownEventType;

const BARE: EventType = EventType::new(Family::new(""), TypeName::new("Bare"));
const NESTED: EventType =
    EventType::new(Family::new("sqlx.round-trip.deep"), TypeName::new("Nested"));
const ARM: EventType = EventType::with_variant(
    Family::new("sqlx.round-trip"),
    TypeName::new("Choice"),
    Variant::new("Left"),
);

#[tokio::test]
async fn occurred_at_round_trips_at_the_microsecond_resolution_the_column_holds() {
    const STREAM: &str = "sqlx.round-trip.occurred-at";

    let Some(backend) = harness::postgres_backend(&[fixture::RECORDED]).await else {
        return;
    };
    let store = backend.bring_up_store().await.store;

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
async fn every_shape_of_event_type_round_trips_through_the_registry() {
    const STREAM: &str = "sqlx.round-trip.event-type";

    let Some(backend) = harness::postgres_backend(&[BARE, NESTED, ARM]).await else {
        return;
    };
    let store = backend.bring_up_store().await.store;
    committed(
        &store,
        STREAM,
        vec![
            typed(1, BARE, b"bare"),
            typed(2, NESTED, b"nested"),
            typed(3, ARM, b"arm"),
        ],
    )
    .await;

    let served = observe(&store, LoadQuery::by_stream(STREAM)).await;

    let round_tripped: Vec<EventType> = served.iter().map(|event| event.event_type).collect();
    assert_eq!(
        round_tripped,
        vec![BARE, NESTED, ARM],
        "an event type is restored as the registered value it was stored from, an empty family \
         and an enum arm included"
    );
}

#[tokio::test]
async fn a_row_carrying_an_unregistered_event_type_fails_its_load_instead_of_vanishing() {
    const STREAM: &str = "sqlx.round-trip.unregistered";

    let Some(backend) = harness::postgres_backend(&[fixture::RECORDED]).await else {
        return;
    };
    let written = backend.bring_up_store().await;
    committed(&written.store, STREAM, vec![fixture::recorded(1, b"e1")]).await;

    let blind = backend.store_on(&written.schema, &[]).await;
    let mut rows = blind
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
                    "a row whose event type is not registered fails with the path it carried, and \
                     the store reported {failure}"
                )
            };
            assert_eq!(
                unknown.path,
                fixture::RECORDED.to_path().to_string(),
                "the failure names the canonical path the row carried"
            );
        }
        Ok(event) => panic!(
            "a row whose event type is not registered cannot be restored, and the store served it \
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
