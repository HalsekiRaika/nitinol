//! `EventSourceSystem::spawn_aggregate` accepts `Arc<dyn EventStore>` (Issue #40).
//!
//! The system-level convenience methods follow the same wiring change as
//! `AggregateProps::new`: the second argument is `Arc<dyn EventStore>`
//! (previously `EventPersistorProxy`).

use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use serde::{Deserialize, Serialize};

use nitinol_eventsource::{
    codec::Codec, system::EventSourceSystem, Aggregate, Context, Decider, Effect, Event,
};
use nitinol_persistence::store::{EventStore, InMemoryEventStore};
use nitinol_persistence::{AggregateId, EventType};
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
    const EVENT_TYPE: EventType = EventType::from_str("e2e.system.Bumped");
}

impl Aggregate for Counter {
    type Event = Bumped;
    fn apply(&mut self, _event: Bumped) {}
}

struct Bump;

#[async_trait]
impl Decider<Bump> for Counter {
    type Rejection = std::convert::Infallible;

    async fn decide(
        &self,
        _cmd: Bump,
        _ctx: &mut Context,
    ) -> Result<Effect<Bumped>, Self::Rejection> {
        Ok(Effect::persist(Bumped))
    }
}

// ---------------------------------------------------------------------------
// Runtime: EventSourceSystem::spawn_aggregate takes Arc<dyn EventStore>
// ---------------------------------------------------------------------------

/// `spawn_aggregate(id, store)` works with `Arc<dyn EventStore>` —
/// confirming the wiring at the system convenience layer matches the
/// underlying `AggregateProps::new` signature change.
#[tokio::test]
async fn spawn_aggregate_accepts_arc_dyn_event_store() {
    // Given
    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::new(ps)
        .with_codec::<EventSourceSystemCodec>()
        .build();
    let store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());

    // When
    let proxy = system
        .spawn_aggregate::<Counter>(AggregateId::new("system-direct-store"), store)
        .await;

    // Then
    let events = proxy.ask(Bump).await.expect("ask must succeed");
    assert_eq!(events, vec![Bumped]);
}

// ---------------------------------------------------------------------------
// Runtime: aggregate_props (no spawn) accepts Arc<dyn EventStore>
// ---------------------------------------------------------------------------

/// `aggregate_props(id, store)` returns a builder pre-wired to the store.
/// The caller can attach further configuration before spawning.
#[tokio::test]
async fn aggregate_props_helper_accepts_arc_dyn_event_store() {
    // Given
    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::new(ps)
        .with_codec::<EventSourceSystemCodec>()
        .build();
    let store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());

    // When: obtain a configured builder, then spawn manually
    let props = system.aggregate_props::<Counter>(AggregateId::new("system-props-direct"), store);
    let proxy = props.spawn(system.process_system()).await;

    // Then
    proxy.ask(Bump).await.expect("ask must succeed");
}
