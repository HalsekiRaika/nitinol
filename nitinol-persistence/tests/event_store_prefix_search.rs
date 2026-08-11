//! Integration tests for [`LoadQuery`] hierarchical prefix search.

use std::borrow::Borrow;

use bytes::Bytes;
use futures_util::TryStreamExt;
use jiff::Timestamp;
use nitinol_persistence::store::{EventStore, InMemoryEventStore};
use nitinol_persistence::{
    AggregateId, AppendingEvent, EventType, Family, LoadQuery, MaterializedPath, TypeName, Variant,
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

/// Order-domain event types sharing a family so hierarchy is exercised.
fn order_placed() -> EventType {
    EventType::with_variant(
        Family::new("saga.upstream"),
        TypeName::new("OrderEvent"),
        Variant::new("OrderPlaced"),
    )
}

fn order_cancelled() -> EventType {
    EventType::with_variant(
        Family::new("saga.upstream"),
        TypeName::new("OrderEvent"),
        Variant::new("OrderCancelled"),
    )
}

fn payment_captured() -> EventType {
    EventType::new(
        Family::new("saga.downstream"),
        TypeName::new("PaymentCaptured"),
    )
}

/// Append the three fixture events (two upstream OrderEvent arms + one
/// downstream PaymentCaptured) to a single stream and return the store.
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
/// only events within that family (all its type names and variants) are
/// returned.
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

    // Then: both OrderEvent arms match; the downstream PaymentCaptured does not
    assert_eq!(types.len(), 2);
    assert!(types.contains(&order_placed()));
    assert!(types.contains(&order_cancelled()));
    assert!(!types.contains(&payment_captured()));
}

/// Given enum-arm events sharing a type name, When queried by the type-level
/// prefix (family + type name, no variant), Then every arm of that type is
/// returned.
#[tokio::test]
async fn prefix_search_by_type_name_returns_all_variants() {
    // Given
    let agg = AggregateId::new("agg-1");
    let store = seeded_store(&agg).await;

    // When: query `saga.upstream.OrderEvent` (all arms)
    let types = load_all(
        &store,
        LoadQuery::by_event_type_prefix(prefix("saga.upstream.OrderEvent")),
    )
    .await;

    // Then: both OrderPlaced and OrderCancelled arms match
    assert_eq!(types.len(), 2);
    assert!(types.contains(&order_placed()));
    assert!(types.contains(&order_cancelled()));
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

/// Given a full-identity EventType with a variant, When queried via
/// `by_event_type`, Then only that exact arm is returned and its sibling
/// variant is excluded.
///
/// `by_event_type` converts the EventType to its Materialized Path and uses
/// it as a prefix. A full arm path such as `saga.upstream.OrderEvent.OrderPlaced`
/// has no descendant paths in the store, so it degenerates to a single-match
/// result.
#[tokio::test]
async fn by_event_type_exact_match_with_variant() {
    // Given
    let agg = AggregateId::new("agg-1");
    let store = seeded_store(&agg).await;

    // When: query the exact OrderPlaced identity
    let types = load_all(&store, LoadQuery::by_event_type(order_placed())).await;

    // Then: only OrderPlaced matches — its sibling arm and the downstream event
    // are excluded even though they share ancestors.
    assert_eq!(types, vec![order_placed()]);
}

/// Given a type-level (no-variant) EventType, When queried via `by_event_type`,
/// Then all variant descendants of that type are returned (prefix semantics:
/// `saga.upstream.OrderEvent` is an ancestor of every arm under that type).
#[tokio::test]
async fn by_event_type_type_level_returns_all_variant_descendants() {
    // Given: store with two variant events (OrderPlaced, OrderCancelled) sharing
    // a type name, plus a downstream event
    let agg = AggregateId::new("agg-1");
    let store = seeded_store(&agg).await;

    // When: query with the type-level identity (no variant)
    let type_only = EventType::new(Family::new("saga.upstream"), TypeName::new("OrderEvent"));
    let types = load_all(&store, LoadQuery::by_event_type(type_only)).await;

    // Then: both arms match — `saga.upstream.OrderEvent` is an ancestor prefix
    // of `saga.upstream.OrderEvent.OrderPlaced` and `…OrderCancelled`.
    // The downstream PaymentCaptured is excluded (different family).
    assert_eq!(types.len(), 2);
    assert!(types.contains(&order_placed()));
    assert!(types.contains(&order_cancelled()));
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
