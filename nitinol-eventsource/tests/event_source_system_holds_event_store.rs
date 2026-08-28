//! `EventSourceSystem::spawn_aggregate` accepts `Arc<dyn EventStore>`.
//!
//! The system-level convenience methods follow the same wiring change as
//! `AggregateProps::new`: the second argument is `Arc<dyn EventStore>`
//! (previously `EventPersistorProxy`).

use std::sync::Arc;

use bytes::Bytes;
use futures_util::TryStreamExt;
use serde::{Deserialize, Serialize};

use nitinol_eventsource::{
    codec::Codec, system::EventSourceSystem, Aggregate, Decider, Decision, Event,
};
use nitinol_persistence::store::{EventStore, InMemoryEventStore};
use nitinol_persistence::{AggregateId, EventType, Family, LoadQuery, TypeName};
use nitinol_runtime::ProcessSystem;

#[derive(Default)]
struct EventSourceSystemCodec;

impl<E: Serialize + for<'de> Deserialize<'de>> Codec<E> for EventSourceSystemCodec {
    type Error = serde_json::Error;

    fn encode(event: &E) -> Result<Bytes, Self::Error> {
        serde_json::to_vec(event).map(Bytes::from)
    }

    fn decode(payload: &[u8]) -> Result<E, Self::Error> {
        serde_json::from_slice(payload)
    }
}

#[derive(Default)]
struct Counter;

#[derive(Clone, Serialize, Deserialize, Debug, PartialEq)]
struct Bumped;

impl Event for Bumped {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("e2e.system"), TypeName::new("Bumped"));
}

impl Aggregate for Counter {
    type Event = Bumped;
    fn apply(&mut self, _event: Bumped) {}
}

struct Bump;

impl Decider<Bump> for Counter {
    type Output = ();
    type Rejection = std::convert::Infallible;

    fn decide(&self, _cmd: Bump) -> Decision<Bumped, (), Self::Rejection> {
        Decision::persist(vec![Bumped]).output(())
    }
}

/// Event types recorded under `id` in `store`, in stored order.
async fn stream_event_types(store: &Arc<dyn EventStore>, id: &AggregateId) -> Vec<EventType> {
    let loaded: Vec<_> = store
        .load(LoadQuery::by_stream(id))
        .await
        .expect("load must succeed")
        .try_collect()
        .await
        .expect("collect must succeed");
    loaded.into_iter().map(|event| event.event_type).collect()
}

// Runtime: EventSourceSystem::spawn_aggregate takes Arc<dyn EventStore>

/// `spawn_aggregate(id, store)` works with `Arc<dyn EventStore>` —
/// confirming the wiring at the system convenience layer matches the
/// underlying `AggregateProps::new` signature change.
#[tokio::test]
async fn spawn_aggregate_accepts_arc_dyn_event_store() {
    // Given
    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::builder(ps)
        .with_codec::<EventSourceSystemCodec>()
        .build();
    let store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let id = AggregateId::new("system-direct-store");

    // When
    let proxy = system
        .spawn_aggregate::<Counter>(id.clone(), Arc::clone(&store))
        .await;

    // Then: the command reaches the aggregate and its fact lands on that store
    proxy.ask(Bump).await.expect("ask must succeed");
    assert_eq!(
        stream_event_types(&store, &id).await,
        vec![Bumped::EVENT_TYPE]
    );
}

// Runtime: aggregate_props (no spawn) accepts Arc<dyn EventStore>

/// `aggregate_props(id, store)` returns a builder pre-wired to the store.
/// The caller can attach further configuration before spawning.
#[tokio::test]
async fn aggregate_props_helper_accepts_arc_dyn_event_store() {
    // Given
    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::builder(ps)
        .with_codec::<EventSourceSystemCodec>()
        .build();
    let store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());

    // When: obtain a configured builder, then spawn manually
    let props = system.aggregate_props::<Counter>(AggregateId::new("system-props-direct"), store);
    let proxy = props.spawn(system.process_system()).await;

    // Then
    proxy.ask(Bump).await.expect("ask must succeed");
}
