// A reference to an aggregate from `(kind, id)` alone, without awaiting.
//
// The saga boundaries that replace the fan-out example's hand-written router are
// synchronous — a crash-restart factory and a `fold` closure — so they cannot
// await an activation.  The entry point they need therefore has to hand back an
// `AggregateProxy` synchronously, and the reference it returns has to address the
// same aggregate as every other entry point (R-3).
//
// The synchronous half of that contract is checked by construction: the call
// below happens inside a non-async closure, which stops compiling the moment the
// entry point becomes an `async fn`.

use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use futures_util::TryStreamExt;
use serde::{Deserialize, Serialize};

use nitinol_eventsource::system::EventSourceSystem;
use nitinol_eventsource::{
    codec::Codec, Aggregate, AggregateProxy, Context, Decider, Effect, Event,
};
use nitinol_persistence::store::{EventStore, InMemoryEventStore};
use nitinol_persistence::{AggregateId, EventType, Family, LoadQuery, TypeName};
use nitinol_runtime::ProcessSystem;

// Fixtures: event

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct Incremented;

impl Event for Incremented {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("sync-entry"), TypeName::new("Incremented"));
}

// Fixtures: aggregate

#[derive(Default)]
struct Counter {
    value: u64,
}

impl Aggregate for Counter {
    type Event = Incremented;

    fn apply(&mut self, _event: Incremented) {
        self.value += 1;
    }
}

// Fixtures: command

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

// Fixtures: codec

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

// Helpers

/// Stream sequences stored under `id`, in ascending order.
async fn stored_sequences(store: &Arc<dyn EventStore>, id: &AggregateId) -> Vec<u64> {
    let loaded: Vec<_> = store
        .load(LoadQuery::by_stream(id))
        .await
        .expect("load must succeed")
        .try_collect()
        .await
        .expect("collecting the stream must succeed");
    loaded.iter().map(|event| event.sequence).collect()
}

// IMP-1

/// A reference built from `(kind, id)` in a synchronous context addresses the
/// same aggregate as the awaited entry points.
///
/// The last dispatch is the discriminator: if the synchronous reference had
/// produced an activation of its own, its sequence counter would already have
/// been overtaken by the one `resolved` used.
#[tokio::test]
async fn synchronously_built_reference_addresses_the_same_aggregate() {
    // Given
    let ps = ProcessSystem::new().await;
    let store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let system = EventSourceSystem::builder(ps)
        .with_codec::<JsonCodec>()
        .with_event_store(Arc::clone(&store))
        .build();
    let id = AggregateId::new("sync-entry-shared");

    // A non-async closure: this compiles only while the entry point is synchronous.
    let reference_for = |key: &str| system.aggregate_proxy::<Counter>(AggregateId::new(key));

    // When
    let reference: AggregateProxy<Counter> = reference_for(id.as_str());
    reference
        .ask(Increment)
        .await
        .expect("a synchronously built reference must dispatch");

    let resolved = system.spawn_aggregate::<Counter>(id.clone()).await;
    resolved
        .ask(Increment)
        .await
        .expect("the awaited entry point must reach the same aggregate");

    reference
        .ask(Increment)
        .await
        .expect("the synchronous reference must still be writing the same stream");

    // Then
    assert_eq!(
        stored_sequences(&store, &id).await,
        vec![1, 2, 3],
        "both entry points must feed one writer"
    );
    assert_eq!(
        reference.aggregate_id().as_str(),
        id.as_str(),
        "the reference must carry the stream key it was built from"
    );
}
