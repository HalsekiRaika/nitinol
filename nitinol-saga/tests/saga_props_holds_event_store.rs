//! `SagaProps::new` accepts `Arc<dyn EventStore>` directly (Issue #40).
//!
//! Mirrors the aggregate-side refactor: the saga process now holds the
//! event store inline and calls `store.append` / `store.load` against its
//! own stream (keyed by `SagaId.borrow()`), with no `EventPersistor` actor
//! between it and persistence.
//!
//! This test deliberately keeps fixtures minimal — it only verifies the
//! shape of `SagaProps::new` and that spawn does not panic.  Full saga
//! end-to-end behavior is covered by `saga_replay_with_direct_store.rs`.

mod common;
use common::JsonCodec;

use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use nitinol_eventsource::{
    system::EventSourceSystem, Event, EventEnvelope,
};
use nitinol_persistence::store::{EventStore, InMemoryEventStore};
use nitinol_persistence::EventType;
use nitinol_runtime::ident::ProcessName;
use nitinol_runtime::ProcessSystem;
use nitinol_saga::{Saga, SagaContext, SagaEffect, SagaId, SagaProps};

// ---------------------------------------------------------------------------
// Minimal subscribed event + saga
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Triggered;

impl Event for Triggered {
    const EVENT_TYPE: EventType = EventType::from_str("saga.direct.Triggered");
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Recorded;

impl Event for Recorded {
    const EVENT_TYPE: EventType = EventType::from_str("saga.direct.Recorded");
}

#[derive(Default)]
struct TrivialSaga;

#[async_trait]
impl Saga for TrivialSaga {
    type SubscribedEvent = Triggered;
    type Event = Recorded;
    type State = ();
    type Error = std::convert::Infallible;

    fn apply(&mut self, _event: Self::Event) {}

    async fn handle(
        &mut self,
        _event: Self::SubscribedEvent,
        _ctx: &mut SagaContext,
    ) -> Result<SagaEffect<Self::Event>, Self::Error> {
        Ok(SagaEffect::None)
    }
}

// ---------------------------------------------------------------------------
// Runtime: SagaProps::new accepts Arc<dyn EventStore>
// ---------------------------------------------------------------------------

/// SagaProps takes Arc<dyn EventStore> as its 2nd argument — the saga
/// process owns the store reference directly.
#[tokio::test]
async fn saga_props_spawns_with_arc_dyn_event_store() {
    // Given
    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::new(ps).with_codec::<JsonCodec>().build();
    let store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());

    let stream = system
        .process_system()
        .spawn_stream::<EventEnvelope<Triggered>>(ProcessName::new("saga-props-direct-stream"))
        .await
        .expect("spawn_stream must succeed");

    let saga_id = SagaId::new("saga-props-direct");
    let routed = saga_id.clone();
    let route_fn = move |_event: &Triggered| -> Option<SagaId> { Some(routed.clone()) };

    // When: SagaProps with Arc<dyn EventStore>
    let _proxy = SagaProps::<TrivialSaga>::new(saga_id, store, TrivialSaga::default)
        .with_codec(system.codec::<Recorded>())
        .with_subscription(stream, route_fn)
        .spawn(system.process_system())
        .await;

    // Then: spawn completed without panic
}
