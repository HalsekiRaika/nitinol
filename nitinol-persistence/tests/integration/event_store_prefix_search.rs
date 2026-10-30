//! Integration tests for [`LoadQuery`] hierarchical prefix search.
//!
//! The selection rule is unchanged: `event_type_prefix` selects the path itself
//! and everything beneath it, on `.`-delimited segment boundaries. What changed
//! is what a stored path can be — a family and a type name, with nothing after
//! it — so the fixtures below are the shapes a column now holds.

use std::borrow::Borrow;

use bytes::Bytes;
use futures_util::TryStreamExt;
use jiff::Timestamp;
use nitinol_persistence::store::{EventStore, InMemoryEventStore};
use nitinol_persistence::{
    AggregateId, AppendingEvent, EventType, Family, LoadQuery, MaterializedPath, TypeName,
};

fn make_event(sequence: u64, event_type: EventType, payload: &'static [u8]) -> AppendingEvent {
    AppendingEvent {
        sequence,
        event_type,
        payload: Bytes::from_static(payload),
        occurred_at: Timestamp::now(),
    }
}

fn prefix(s: &str) -> MaterializedPath {
    s.parse::<MaterializedPath>()
        .unwrap_or_else(|_| panic!("`{s}` must parse as a MaterializedPath"))
}

/// Two sibling declarations of one family, so the family level has more than one
/// descendant to gather.
fn order_placed() -> EventType {
    EventType::new(Family::new("saga.upstream"), TypeName::new("OrderPlaced"))
}

fn order_cancelled() -> EventType {
    EventType::new(
        Family::new("saga.upstream"),
        TypeName::new("OrderCancelled"),
    )
}

/// A declaration of a different family, which no `saga.upstream` prefix reaches.
fn payment_captured() -> EventType {
    EventType::new(
        Family::new("saga.downstream"),
        TypeName::new("PaymentCaptured"),
    )
}

/// Append the three fixture events (two upstream siblings + one downstream) to a
/// single stream and return the store.
async fn seeded_store(agg: &AggregateId) -> InMemoryEventStore {
    let store = InMemoryEventStore::default();
    store
        .append(
            agg.borrow(),
            vec![
                make_event(1, order_placed(), b"placed"),
                make_event(2, payment_captured(), b"captured"),
                make_event(3, order_cancelled(), b"cancelled"),
            ],
        )
        .await
        .expect("append should succeed");
    store
}

async fn load_all(store: &InMemoryEventStore, query: LoadQuery) -> Vec<EventType> {
    let stream = store.load(query).await.expect("load should succeed");
    let events: Vec<_> = stream.try_collect().await.expect("collect should succeed");
    events.into_iter().map(|e| e.event_type).collect()
}

/// Given events under two families, When queried by a family-level prefix, Then
/// only events declared within that family are returned.
#[tokio::test]
async fn prefix_search_by_family_returns_all_descendants() {
    // Given
    let agg = AggregateId::new("agg-1");
    let store = seeded_store(&agg).await;

    // When: query the whole `saga.upstream` family
    let types = load_all(
        &store,
        LoadQuery::by_event_type_prefix(prefix("saga.upstream")),
    )
    .await;

    // Then: both upstream declarations match; the downstream one does not
    assert_eq!(types.len(), 2);
    assert!(types.contains(&order_placed()));
    assert!(types.contains(&order_cancelled()));
    assert!(!types.contains(&payment_captured()));
}

/// Given a prefix that shares a raw string prefix but crosses no segment
/// boundary, When queried, Then no events match (segment-boundary semantics,
/// not raw string prefix).
#[tokio::test]
async fn prefix_search_respects_segment_boundaries() {
    // Given
    let agg = AggregateId::new("agg-1");
    let store = seeded_store(&agg).await;

    // When: `saga.up` is a raw-string prefix of `saga.upstream` but not a
    // segment-aligned ancestor.
    let types = load_all(&store, LoadQuery::by_event_type_prefix(prefix("saga.up"))).await;

    // Then: nothing matches
    assert!(types.is_empty());
}

/// Given an EventType, When queried via `by_event_type`, Then the rows carrying
/// that event type are returned and its family siblings are not.
///
/// A canonical path is a family and a type name, so an event type's own path is
/// what a row of it carries — there is no deeper path for the same declaration
/// to hide under, and no shallower one that would gather its siblings too.
#[tokio::test]
async fn by_event_type_returns_the_rows_of_that_event_type_and_no_sibling() {
    // Given
    let agg = AggregateId::new("agg-1");
    let store = seeded_store(&agg).await;

    // When: query one declared identity
    let types = load_all(&store, LoadQuery::by_event_type(order_placed())).await;

    // Then: only that declaration's row is served
    assert_eq!(types, vec![order_placed()]);
}

/// Given a family that no declaration in the store belongs to, When queried by
/// that event type, Then nothing is returned.
#[tokio::test]
async fn by_event_type_returns_nothing_for_an_undeclared_identity() {
    let agg = AggregateId::new("agg-1");
    let store = seeded_store(&agg).await;

    let absent = EventType::new(Family::new("saga.upstream"), TypeName::new("OrderAmended"));
    let types = load_all(&store, LoadQuery::by_event_type(absent)).await;

    assert!(types.is_empty());
}

/// Given the prefix filter, When combined with `with_limit`, Then the limit is
/// applied on top of the prefix match (axes compose).
#[tokio::test]
async fn prefix_search_composes_with_limit() {
    // Given
    let agg = AggregateId::new("agg-1");
    let store = seeded_store(&agg).await;

    // When: family prefix (2 matches) with limit 1
    let query = LoadQuery::by_event_type_prefix(prefix("saga.upstream")).with_limit(1);
    let types = load_all(&store, query).await;

    // Then: at most one event is returned
    assert_eq!(types.len(), 1);
    assert!(types[0] == order_placed() || types[0] == order_cancelled());
}

/// Given events on two streams, When a prefix query is issued without a stream
/// filter, Then it spans all streams (prefix is a global filter, like the
/// existing event-type axis).
#[tokio::test]
async fn prefix_search_spans_all_streams() {
    // Given: OrderPlaced on agg-1, OrderCancelled on agg-2
    let store = InMemoryEventStore::default();
    let agg1 = AggregateId::new("agg-1");
    let agg2 = AggregateId::new("agg-2");
    store
        .append(agg1.borrow(), vec![make_event(1, order_placed(), b"p")])
        .await
        .expect("append agg1 should succeed");
    store
        .append(agg2.borrow(), vec![make_event(1, order_cancelled(), b"c")])
        .await
        .expect("append agg2 should succeed");

    // When: family-level prefix across the whole store
    let types = load_all(
        &store,
        LoadQuery::by_event_type_prefix(prefix("saga.upstream")),
    )
    .await;

    // Then: events from both streams are returned
    assert_eq!(types.len(), 2);
    assert!(types.contains(&order_placed()));
    assert!(types.contains(&order_cancelled()));
}
