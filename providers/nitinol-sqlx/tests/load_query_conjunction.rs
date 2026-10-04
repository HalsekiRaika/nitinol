//! An event-type prefix and a global position, asked for together.
//!
//! ```gherkin
//!   Scenario: both conditions at once
//!     Given stored rows inside and outside a prefix, above and below a position
//!     When a load asks for that prefix from that position
//!     Then it serves the rows satisfying both, in ascending global order
//!
//!   Scenario: a prefix that matches nothing
//!     Given a position every stored row lies above
//!     When a load asks for a prefix no stored row is within
//!     Then it serves nothing
//! ```
//!
//! This is the pair a consumer that follows one event type across every stream
//! asks for: its own type, above the position it has finished with. A store
//! that answered either condition alone would hand such a consumer rows it
//! cannot read, or rows it has already finished with.
//!
//! The neighbouring paths are what make this more than a restatement of the
//! prefix clause on its own. A prefix condition can be made to use an index by
//! being rewritten as a range over the column, and `_` and `%` — both legal in
//! a family — are where a rewrite through a pattern match stops being
//! equivalent. Keeping the two conditions together in one query is also where a
//! rewrite can quietly change which rows come back, so the neighbours are
//! asked for here with the position condition in place.
//!
//! Running any of this needs a database. Where none can be reached the clause
//! judges nothing, which is not evidence that it holds.

#[path = "common/fixture.rs"]
mod fixture;
#[path = "common/harness.rs"]
mod harness;

use fixture::{committed, highest, observe, payloads, typed};
use linkme::distributed_slice;
use nitinol_persistence::{
    EventRegistryEntry, EventType, Family, LoadQuery, MaterializedPath, TypeName, EVENT_REGISTRY,
};

const SELECTED: EventType = EventType::new(Family::new("conj.prefix"), TypeName::new("Selected"));
const DESCENDANT: EventType =
    EventType::new(Family::new("conj.prefix.deeper"), TypeName::new("Selected"));
const SIBLING: EventType = EventType::new(Family::new("conj.prefixed"), TypeName::new("Selected"));
const UNDERSCORE: EventType =
    EventType::new(Family::new("conj.prefix_"), TypeName::new("Selected"));
const PERCENT: EventType = EventType::new(Family::new("conj.prefix%"), TypeName::new("Selected"));

#[distributed_slice(EVENT_REGISTRY)]
static CONJUNCTION_PATHS: EventRegistryEntry = EventRegistryEntry::new(
    concat!(module_path!(), "::conjunction_paths"),
    &[SELECTED, DESCENDANT, SIBLING, UNDERSCORE, PERCENT],
);

const WITHIN: &str = "conj.prefix";

/// Two streams, so neither event type owns one and no condition can be
/// satisfied by the stream key instead.
const FIRST_STREAM: &str = "conj.stream-a";
const SECOND_STREAM: &str = "conj.stream-b";

fn prefix(path: &str) -> MaterializedPath {
    path.parse()
        .expect("a dotted path with no empty segment is a materialized path")
}

#[tokio::test]
async fn a_prefix_and_a_position_are_both_required() {
    let Some(backend) = harness::postgres_backend().await else {
        return;
    };
    let store = backend.bring_up_store().await;

    // Rows inside and outside the prefix, interleaved, so that the position
    // below has rows of both kinds on either side of it.
    let below = highest(
        committed(
            &store,
            FIRST_STREAM,
            vec![typed(1, SELECTED, b"below-within")],
        )
        .await,
    );
    committed(
        &store,
        SECOND_STREAM,
        vec![typed(1, SIBLING, b"below-outside")],
    )
    .await;
    let floor = highest(
        committed(
            &store,
            FIRST_STREAM,
            vec![typed(2, DESCENDANT, b"at-floor-within")],
        )
        .await,
    );
    committed(
        &store,
        SECOND_STREAM,
        vec![
            typed(2, UNDERSCORE, b"above-outside-underscore"),
            typed(3, PERCENT, b"above-outside-percent"),
        ],
    )
    .await;
    committed(
        &store,
        FIRST_STREAM,
        vec![typed(3, SELECTED, b"above-within")],
    )
    .await;

    let served = observe(
        &store,
        LoadQuery {
            event_type_prefix: Some(prefix(WITHIN)),
            from_global_sequence: Some(floor),
            ..LoadQuery::default()
        },
    )
    .await;

    assert_eq!(
        payloads(&served),
        vec![b"at-floor-within".as_slice(), b"above-within".as_slice()],
        "the rows served are the ones within the prefix and at or above the \
         position. A row within the prefix below the position, and rows above \
         the position whose paths only share the prefix's characters, are both \
         left out"
    );
    assert!(
        served
            .iter()
            .all(|event| event.global_sequence >= floor && event.global_sequence != below),
        "the position condition applies to every row served"
    );
    assert!(
        served
            .windows(2)
            .all(|pair| pair[0].global_sequence < pair[1].global_sequence),
        "and what is served is in ascending global order, which is what lets a \
         consumer take the last row's position as its progress"
    );
}

#[tokio::test]
async fn a_canonical_prefix_with_a_position_serves_that_type_alone() {
    let Some(backend) = harness::postgres_backend().await else {
        return;
    };
    let store = backend.bring_up_store().await;

    let floor = highest(
        committed(
            &store,
            FIRST_STREAM,
            vec![
                typed(1, SELECTED, b"below"),
                typed(2, SELECTED, b"at-floor"),
            ],
        )
        .await,
    );
    committed(
        &store,
        SECOND_STREAM,
        vec![typed(1, DESCENDANT, b"descendant-above")],
    )
    .await;

    let served = observe(
        &store,
        LoadQuery {
            event_type_prefix: Some(SELECTED.to_path()),
            from_global_sequence: Some(floor),
            ..LoadQuery::default()
        },
    )
    .await;

    assert_eq!(
        payloads(&served),
        vec![b"at-floor".as_slice()],
        "a canonical path has no descendant of its own, so the pair selects one \
         declaration's rows above the position and nothing else — which is what \
         a consumer of a single event type asks for"
    );
}

#[tokio::test]
async fn a_prefix_matching_nothing_serves_nothing_whatever_the_position() {
    let Some(backend) = harness::postgres_backend().await else {
        return;
    };
    let store = backend.bring_up_store().await;

    committed(
        &store,
        FIRST_STREAM,
        vec![
            typed(1, SELECTED, b"one"),
            typed(2, DESCENDANT, b"two"),
            typed(3, SIBLING, b"three"),
        ],
    )
    .await;

    for floor in [1u64, 2, 3] {
        let served = observe(
            &store,
            LoadQuery {
                event_type_prefix: Some(prefix("conj.absent")),
                from_global_sequence: Some(floor),
                ..LoadQuery::default()
            },
        )
        .await;

        assert!(
            served.is_empty(),
            "no stored row is within the prefix, so no position can admit one: \
             the conditions are a conjunction rather than an alternative. From \
             {floor} the store served {} rows",
            served.len()
        );
    }
}
