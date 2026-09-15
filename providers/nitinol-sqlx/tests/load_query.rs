//! What a load serves for the query it was given.
//!
//! ```gherkin
//!   Scenario: an event type prefix
//!     Given stored event types whose paths share the prefix's characters
//!          without sharing its segments
//!     When a load asks for that prefix
//!     Then it serves the prefix's own path and its descendants, and nothing
//!          that merely starts with the same characters
//!
//!   Scenario: the conditions of a query
//!     Then a load serves the stream, the checkpoint, the stream sequence and
//!          the limit it was asked for
//! ```
//!
//! A prefix compared as a pattern rather than by segment is the failure the
//! first scenario is built around: `_` and `%` are wildcards to a pattern
//! match, and both are legal in a family.
//!
//! Running any of this needs a database. Where none can be reached the clause
//! judges nothing, which is not evidence that it holds.

#[path = "common/fixture.rs"]
mod fixture;
#[path = "common/harness.rs"]
mod harness;

use fixture::{committed, highest, observe, payloads, recorded, typed};
use nitinol_persistence::{EventType, Family, LoadQuery, MaterializedPath, TypeName};

const UNDER: EventType = EventType::new(Family::new("sqlx.prefix"), TypeName::new("Under"));
const DEEPER: EventType = EventType::new(Family::new("sqlx.prefix.deeper"), TypeName::new("Under"));
const SIBLING: EventType = EventType::new(Family::new("sqlx.prefixed"), TypeName::new("Under"));
const UNDERSCORE: EventType = EventType::new(Family::new("sqlx.prefix_"), TypeName::new("Under"));
const PERCENT: EventType = EventType::new(Family::new("sqlx.prefix%"), TypeName::new("Under"));

#[tokio::test]
async fn an_event_type_prefix_matches_on_segment_boundaries_only() {
    const STREAM: &str = "sqlx.query.prefix";

    let Some(backend) =
        harness::postgres_backend(&[UNDER, DEEPER, SIBLING, UNDERSCORE, PERCENT]).await
    else {
        return;
    };
    let store = backend.bring_up_store().await.store;
    committed(
        &store,
        STREAM,
        vec![
            typed(1, UNDER, b"under"),
            typed(2, DEEPER, b"deeper"),
            typed(3, SIBLING, b"sibling"),
            typed(4, UNDERSCORE, b"underscore"),
            typed(5, PERCENT, b"percent"),
        ],
    )
    .await;

    let prefix: MaterializedPath = "sqlx.prefix"
        .parse()
        .expect("a dotted path with no empty segment is a materialized path");
    let served = observe(&store, LoadQuery::by_event_type_prefix(prefix)).await;

    assert_eq!(
        payloads(&served),
        vec![b"under".as_slice(), b"deeper".as_slice()],
        "a prefix matches its own path and its descendants, and nothing that merely starts with \
         the same characters"
    );
}

#[tokio::test]
async fn a_load_serves_only_what_its_query_asks_for() {
    const STREAM: &str = "sqlx.query.conditions";
    const OTHER: &str = "sqlx.query.conditions.other";

    let Some(backend) = harness::postgres_backend(&[fixture::RECORDED]).await else {
        return;
    };
    let store = backend.bring_up_store().await.store;
    let first =
        highest(committed(&store, STREAM, vec![recorded(1, b"s1"), recorded(2, b"s2")]).await);
    committed(&store, OTHER, vec![recorded(1, b"o1")]).await;
    committed(&store, STREAM, vec![recorded(3, b"s3")]).await;

    let by_stream = observe(&store, LoadQuery::by_stream(STREAM)).await;
    assert_eq!(
        payloads(&by_stream),
        vec![b"s1".as_slice(), b"s2".as_slice(), b"s3".as_slice()],
        "a query naming a stream serves that stream alone"
    );

    let from_global = observe(&store, LoadQuery::from_global(first + 1)).await;
    assert_eq!(
        payloads(&from_global),
        vec![b"o1".as_slice(), b"s3".as_slice()],
        "a query from a checkpoint serves what lies beyond it, across every stream"
    );

    let from_stream_sequence = observe(
        &store,
        LoadQuery {
            stream_key: Some(STREAM.to_owned()),
            from_stream_sequence: Some(2),
            ..LoadQuery::default()
        },
    )
    .await;
    assert_eq!(
        payloads(&from_stream_sequence),
        vec![b"s2".as_slice(), b"s3".as_slice()],
        "a query from a stream sequence serves that sequence onwards"
    );

    let limited = observe(&store, LoadQuery::default().with_limit(2)).await;
    assert_eq!(
        payloads(&limited),
        vec![b"s1".as_slice(), b"s2".as_slice()],
        "a limit truncates what the other conditions matched, in global order"
    );
}
