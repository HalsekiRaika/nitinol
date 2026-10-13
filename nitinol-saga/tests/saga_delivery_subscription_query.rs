//! What the host asks the store for.
//!
//! ```gherkin
//!   Scenario: the subscription names both conditions
//!     Given a delivery host for a saga type
//!     When it polls the store
//!     Then every query it issues names the decision event type as a prefix
//!     And names the position it has already finished with
//!     And names no single stream
//! ```
//!
//! Naming the event type to the store is what removes a read-and-discard stage:
//! rows of other types never reach the host. Naming no stream is what makes the
//! subscription cross every instance of the saga type, one stream per instance.
//!
//! The query is read off a store that records what it was handed, so what is
//! fixed here is the request itself rather than a result that could be produced
//! either way.

#[path = "common/delivery.rs"]
mod delivery;
use delivery::{
    append_decided, in_memory_system, scope_of, spawn_host, wait_until_loads, wait_until_stored,
    Decided, DeliverySaga, RecordingEventStore, RecordingOffsetStore,
};

use std::sync::Arc;

use nitinol_eventsource::Event;
use nitinol_persistence::store::{EventStore, InMemoryEventStore};
use nitinol_persistence::{AggregateId, LoadQuery};
use nitinol_saga::SagaId;

#[tokio::test]
async fn every_query_names_the_event_type_and_the_position() {
    // Given
    let (system, _aggregates) = in_memory_system().await;
    let recording = RecordingEventStore::new(Arc::new(InMemoryEventStore::default()));
    let journal: Arc<dyn EventStore> = recording.clone();
    let offsets = RecordingOffsetStore::new();
    let saga_id = SagaId::new("instance-1");
    let target = AggregateId::new("ledger-subscription-query");

    append_decided(&journal, &saga_id, 1, target.as_str(), "op-1").await;

    // When
    let _host = spawn_host::<DeliverySaga>(&system, &journal, &offsets, 1).await;
    let queries = wait_until_loads(&recording, 3, "the host must poll the store").await;

    // Then
    let expected_prefix = <Decided as Event>::EVENT_TYPE.to_path();
    for (nth, query) in queries.iter().enumerate() {
        assert_eq!(
            query.event_type_prefix.as_ref(),
            Some(&expected_prefix),
            "load call {nth} must select the decision event type at the store, \
             so no row of another type is fetched to be discarded"
        );
        assert!(
            query.stream_key.is_none(),
            "load call {nth} must name no single stream: one saga type's \
             instances each own a stream, and the host subscribes across all of \
             them"
        );
        assert!(
            query.from_global_sequence.is_some(),
            "load call {nth} must name the position it resumes above, so the \
             store is never asked to return the whole journal"
        );
    }

    assert_eq!(
        first_position(&queries),
        Some(1),
        "with no stored progress the host resumes above position zero"
    );
}

#[tokio::test]
async fn the_position_asked_for_follows_the_progress_recorded() {
    // Given
    let (system, _aggregates) = in_memory_system().await;
    let recording = RecordingEventStore::new(Arc::new(InMemoryEventStore::default()));
    let journal: Arc<dyn EventStore> = recording.clone();
    let offsets = RecordingOffsetStore::new();
    let saga_id = SagaId::new("instance-1");
    let target = AggregateId::new("ledger-query-follows-progress");

    let position = append_decided(&journal, &saga_id, 1, target.as_str(), "op-1").await;

    // When
    let _host = spawn_host::<DeliverySaga>(&system, &journal, &offsets, 1).await;
    wait_until_stored(
        &offsets,
        &scope_of(0),
        position,
        "the decision must be delivered and its position recorded",
    )
    .await;
    let queries = wait_until_loads(&recording, 6, "the host must keep polling").await;

    // Then
    assert_eq!(
        queries.last().and_then(|query| query.from_global_sequence),
        Some(position + 1),
        "once a record is finished with, the next poll asks for what lies \
         beyond it rather than re-reading it"
    );
}

fn first_position(queries: &[LoadQuery]) -> Option<u64> {
    queries.first().and_then(|query| query.from_global_sequence)
}
