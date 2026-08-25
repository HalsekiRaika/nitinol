//! The default [`EventStore`] an `EventSourceSystem` is built with, and the
//! per-spawn override that beats it.
//!
//! `EventSourceSystemBuilder::with_event_store` hands the system one store; the
//! store-less `spawn_aggregate` / `aggregate_props` then resolve it at the
//! system boundary instead of making every call site carry the `Arc`.  The
//! store is stream-keyed (`EventStore::append(key, ..)`), so that single
//! instance holds every aggregate's stream side by side — which is what makes
//! one default store sufficient in the first place.
//!
//! Each test observes the store the events actually land on, not the call that
//! spawned them: an implementation that reaches the right process but the wrong
//! store is exactly the failure these pin down.  The override tests assert both
//! halves — present on the store passed explicitly, absent from the default —
//! because "wrote to both" and "wrote only to the default" are distinct wrong
//! answers that a presence-only assertion would let through.

use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use futures_util::TryStreamExt;
use serde::{Deserialize, Serialize};

use nitinol_eventsource::{
    codec::Codec, system::EventSourceSystem, Aggregate, Context, Decider, Effect, Event,
};
use nitinol_persistence::store::{EventStore, InMemoryEventStore};
use nitinol_persistence::{AggregateId, EventType, Family, LoadQuery, TypeName};
use nitinol_runtime::ProcessSystem;

// Fixtures

#[derive(Default)]
struct JsonCodec;

impl<E: Serialize + for<'de> Deserialize<'de>> Codec<E> for JsonCodec {
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

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct Incremented;

impl Event for Incremented {
    const EVENT_TYPE: EventType = EventType::new(
        Family::new("system.default.store"),
        TypeName::new("Incremented"),
    );
}

impl Aggregate for Counter {
    type Event = Incremented;
    fn apply(&mut self, _event: Incremented) {}
}

struct Increment;

#[async_trait]
impl Decider<Increment> for Counter {
    type Rejection = std::convert::Infallible;

    async fn decide(
        &self,
        _cmd: Increment,
        _ctx: &mut Context,
    ) -> Result<Effect<Incremented>, Self::Rejection> {
        Ok(Effect::persist(Incremented))
    }
}

// Helpers

/// Event types recorded under `key` in `store`, in stored order.
///
/// Reading the stream back out of a *named* store is the only way to tell which
/// of two stores a spawn resolved — the proxy returned by `spawn_aggregate`
/// looks identical either way.
async fn stream_event_types(store: &Arc<dyn EventStore>, key: &AggregateId) -> Vec<EventType> {
    let loaded: Vec<_> = store
        .load(LoadQuery::by_stream(key))
        .await
        .expect("load must succeed")
        .try_collect()
        .await
        .expect("collect must succeed");
    loaded.into_iter().map(|event| event.event_type).collect()
}

// Default store: spawn_aggregate

/// `spawn_aggregate::<A>(id)` — no store argument — must persist onto the store
/// the builder was given.
#[tokio::test]
async fn store_less_spawn_aggregate_persists_onto_the_system_default_store() {
    // Given: a system carrying one default store
    let ps = ProcessSystem::new().await;
    let default_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let system = EventSourceSystem::builder(ps)
        .with_codec::<JsonCodec>()
        .with_event_store(Arc::clone(&default_store))
        .build();
    let id = AggregateId::new("default-store-spawn-aggregate");

    // When: spawning without naming a store
    let proxy = system.spawn_aggregate::<Counter>(id.clone()).await;
    proxy.ask(Increment).await.expect("ask must succeed");

    // Then
    assert_eq!(
        stream_event_types(&default_store, &id).await,
        vec![Incremented::EVENT_TYPE],
        "a store-less spawn must append to the store the builder held"
    );
}

// Default store: aggregate_props

/// `aggregate_props::<A>(id)` must pre-wire the same default store, so a caller
/// that needs further configuration before spawning does not have to re-supply
/// it.
#[tokio::test]
async fn store_less_aggregate_props_persists_onto_the_system_default_store() {
    // Given
    let ps = ProcessSystem::new().await;
    let default_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let system = EventSourceSystem::builder(ps)
        .with_codec::<JsonCodec>()
        .with_event_store(Arc::clone(&default_store))
        .build();
    let id = AggregateId::new("default-store-aggregate-props");

    // When: taking the builder, then spawning it manually
    let proxy = system
        .aggregate_props::<Counter>(id.clone())
        .spawn(system.process_system())
        .await;
    proxy.ask(Increment).await.expect("ask must succeed");

    // Then
    assert_eq!(
        stream_event_types(&default_store, &id).await,
        vec![Incremented::EVENT_TYPE],
        "a store-less aggregate_props must carry the store the builder held"
    );
}

// Override beats default: spawn_aggregate_with_store

/// A store named at spawn time must win over the system default — and the
/// default must stay untouched.
#[tokio::test]
async fn spawn_aggregate_with_store_overrides_the_system_default_store() {
    // Given: a system with a default store, plus a second store for this spawn
    let ps = ProcessSystem::new().await;
    let default_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let override_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let system = EventSourceSystem::builder(ps)
        .with_codec::<JsonCodec>()
        .with_event_store(Arc::clone(&default_store))
        .build();
    let id = AggregateId::new("override-store-spawn-aggregate");

    // When
    let proxy = system
        .spawn_aggregate_with_store::<Counter>(id.clone(), Arc::clone(&override_store))
        .await;
    proxy.ask(Increment).await.expect("ask must succeed");

    // Then
    assert_eq!(
        stream_event_types(&override_store, &id).await,
        vec![Incremented::EVENT_TYPE],
        "the explicitly named store must receive the aggregate's events"
    );
    assert_eq!(
        stream_event_types(&default_store, &id).await,
        Vec::<EventType>::new(),
        "the system default must receive nothing once a spawn names its own store"
    );
}

// Override beats default: aggregate_props_with_store

/// The props-returning override must resolve the same way as the spawning one.
#[tokio::test]
async fn aggregate_props_with_store_overrides_the_system_default_store() {
    // Given
    let ps = ProcessSystem::new().await;
    let default_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let override_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let system = EventSourceSystem::builder(ps)
        .with_codec::<JsonCodec>()
        .with_event_store(Arc::clone(&default_store))
        .build();
    let id = AggregateId::new("override-store-aggregate-props");

    // When
    let proxy = system
        .aggregate_props_with_store::<Counter>(id.clone(), Arc::clone(&override_store))
        .spawn(system.process_system())
        .await;
    proxy.ask(Increment).await.expect("ask must succeed");

    // Then
    assert_eq!(
        stream_event_types(&override_store, &id).await,
        vec![Incremented::EVENT_TYPE],
        "the explicitly named store must receive the aggregate's events"
    );
    assert_eq!(
        stream_event_types(&default_store, &id).await,
        Vec::<EventType>::new(),
        "the system default must receive nothing once a caller names its own store"
    );
}

// Compile-fail coverage
//
// Calling the store-less `spawn_aggregate` / `aggregate_props` on a system that
// was never given a default store must be a COMPILE ERROR (the typestate
// guard).  An integration test cannot express that, so it is verified via
// rustdoc compile_fail doctests on the `EventSourceSystem` store-less methods.
// See: nitinol-eventsource/src/system.rs
