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
//!
//!   Scenario: every condition at once
//!     Given two interleaved streams, each holding rows inside and outside a
//!          prefix
//!     When a load names a stream, a global position, a stream sequence, a
//!          prefix and a limit together
//!     Then it serves the first rows, in global order, that satisfy all of them
//!
//!   Scenario: a cursor and a limit
//!     Given two interleaved streams
//!     When a load asks from a stream cursor or a global cursor with a limit
//!     Then it serves the first rows past the cursor, as many as the limit
//! ```
//!
//! The statement a load runs is put together from the conditions its query
//! holds, so each combination of conditions is a statement of its own. The
//! combinations below are the ones the clauses elsewhere do not reach: the two
//! a projector's batched read asks for, and the one holding every condition,
//! where a condition left out of the statement or a value bound against
//! another condition's column changes which rows come back.
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

use super::fixture;
use super::harness;

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

#[tokio::test]
async fn every_condition_at_once_is_a_conjunction_truncated_by_the_limit() {
    let Some(backend) = harness::postgres_backend().await else {
        return;
    };
    let store = backend.bring_up_store().await;

    // The other stream's rows, a row outside the prefix and rows below either
    // floor sit among the rows expected, so a condition the statement leaves
    // out lets a row in ahead of them.
    committed(&store, FIRST_STREAM, vec![typed(1, SELECTED, b"a1")]).await;
    let other_first =
        highest(committed(&store, SECOND_STREAM, vec![typed(1, SELECTED, b"b1")]).await);
    committed(
        &store,
        FIRST_STREAM,
        vec![typed(2, SELECTED, b"a2"), typed(3, DESCENDANT, b"a3")],
    )
    .await;
    let other_second =
        highest(committed(&store, SECOND_STREAM, vec![typed(2, SELECTED, b"b2")]).await);
    committed(
        &store,
        FIRST_STREAM,
        vec![typed(4, SIBLING, b"a4-sibling"), typed(5, SELECTED, b"a5")],
    )
    .await;
    committed(&store, SECOND_STREAM, vec![typed(3, SELECTED, b"b3")]).await;
    committed(
        &store,
        FIRST_STREAM,
        vec![typed(6, DESCENDANT, b"a6"), typed(7, SELECTED, b"a7")],
    )
    .await;

    // The stream sequence is the tighter floor: the global position admits
    // a2 onwards, the stream sequence a3 onwards. The two floors are different
    // numbers, so a value bound against the other's column moves the floor.
    let stream_sequence_binds = observe(
        &store,
        LoadQuery {
            stream_key: Some(FIRST_STREAM.to_owned()),
            event_type_prefix: Some(prefix(WITHIN)),
            from_global_sequence: Some(other_first),
            from_stream_sequence: Some(3),
            limit: Some(3),
        },
    )
    .await;
    assert_eq!(
        payloads(&stream_sequence_binds),
        vec![b"a3".as_slice(), b"a5".as_slice(), b"a6".as_slice()],
        "with every condition present, the rows served are the stream's own, \
         from its third sequence, within the prefix, and the first three of \
         those in global order: a2 is below the stream sequence, a4 is outside \
         the prefix, b3 is another stream's, and a7 is past the limit"
    );

    // The global position is the tighter floor: it admits a4 onwards, the
    // stream sequence a2 onwards.
    let global_position_binds = observe(
        &store,
        LoadQuery {
            stream_key: Some(FIRST_STREAM.to_owned()),
            event_type_prefix: Some(prefix(WITHIN)),
            from_global_sequence: Some(other_second),
            from_stream_sequence: Some(2),
            limit: Some(2),
        },
    )
    .await;
    assert_eq!(
        payloads(&global_position_binds),
        vec![b"a5".as_slice(), b"a6".as_slice()],
        "with every condition present, the global position applies alongside \
         the stream sequence: a2 and a3 are below it, a4 is outside the prefix, \
         b2 and b3 are another stream's, and a7 is past the limit"
    );
}

#[tokio::test]
async fn a_limit_truncates_what_a_cursor_admits() {
    let Some(backend) = harness::postgres_backend().await else {
        return;
    };
    let store = backend.bring_up_store().await;

    committed(&store, FIRST_STREAM, vec![typed(1, SELECTED, b"a1")]).await;
    committed(&store, SECOND_STREAM, vec![typed(1, SELECTED, b"b1")]).await;
    let second_of_first =
        highest(committed(&store, FIRST_STREAM, vec![typed(2, SELECTED, b"a2")]).await);
    committed(&store, SECOND_STREAM, vec![typed(2, SELECTED, b"b2")]).await;
    committed(
        &store,
        FIRST_STREAM,
        vec![typed(3, SELECTED, b"a3"), typed(4, SELECTED, b"a4")],
    )
    .await;

    // What a projector following one stream asks for in a batch.
    let stream_cursor = observe(
        &store,
        LoadQuery {
            stream_key: Some(FIRST_STREAM.to_owned()),
            from_stream_sequence: Some(2),
            ..LoadQuery::default()
        }
        .with_limit(2),
    )
    .await;
    assert_eq!(
        payloads(&stream_cursor),
        vec![b"a2".as_slice(), b"a3".as_slice()],
        "a stream cursor with a limit serves that stream's first rows from the \
         sequence asked for, and no more of them than the limit: a1 is behind \
         the cursor, b2 is another stream's, and a4 is past the limit"
    );

    // What a projector following the global order asks for in a batch.
    let global_cursor = observe(
        &store,
        LoadQuery::from_global(second_of_first).with_limit(2),
    )
    .await;
    assert_eq!(
        payloads(&global_cursor),
        vec![b"a2".as_slice(), b"b2".as_slice()],
        "a global cursor with a limit serves the first rows at or above the \
         position across every stream, and no more of them than the limit: a1 \
         and b1 are behind the cursor, and a3 and a4 are past the limit"
    );
}
