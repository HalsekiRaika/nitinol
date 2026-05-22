use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use serde::{Deserialize, Serialize};

use nitinol_eventsource::codec::Codec;
use nitinol_eventsource::system::EventSourceSystem;
use nitinol_eventsource::{
    Aggregate, Context, Decider, Effect, Event, EventPersistor,
};
use nitinol_persistence::store::{EventStore, InMemoryEventStore};
use nitinol_persistence::{AggregateId, EventType};
use nitinol_runtime::ProcessSystem;
use nitinol_saga::SagaEffect;

// ---------------------------------------------------------------------------
// JsonCodec — shared across all integration tests
// ---------------------------------------------------------------------------

#[derive(Default)]
#[allow(dead_code)]
pub struct JsonCodec;

impl<E: Serialize + for<'de> Deserialize<'de>> Codec<E> for JsonCodec {
    type Error = serde_json::Error;

    fn encode(event: &E) -> Result<Bytes, Self::Error> {
        serde_json::to_vec(event).map(Bytes::from)
    }

    fn decode(payload: &[u8]) -> Result<E, Self::Error> {
        serde_json::from_slice(payload)
    }
}

// ---------------------------------------------------------------------------
// Minimal aggregate used only to obtain an AggregateProxy for Tell tests.
// ---------------------------------------------------------------------------

#[derive(Default)]
#[allow(dead_code)]
pub struct TestTarget;

#[derive(Serialize, Deserialize, Clone, Debug)]
#[allow(dead_code)]
pub struct TestTargetEvent;

impl Event for TestTargetEvent {
    const EVENT_TYPE: EventType = EventType::from_str("test.noop_target");
}

impl Aggregate for TestTarget {
    type Event = TestTargetEvent;

    fn apply(&mut self, _event: TestTargetEvent) {}
}

#[allow(dead_code)]
pub struct NoopCmd;

#[async_trait]
impl Decider<NoopCmd> for TestTarget {
    type Rejection = std::convert::Infallible;

    async fn decide(
        &self,
        _cmd: NoopCmd,
        _ctx: &mut Context,
    ) -> Result<Effect<TestTargetEvent>, Self::Rejection> {
        Ok(Effect::empty())
    }
}

/// Spin up a minimal system and return a `Tell` saga effect.
///
/// Used by unit tests that need to exercise the `Tell` variant of `SagaEffect`
/// without triggering any real side effect.
#[allow(dead_code)]
pub async fn make_tell_effect<E>() -> SagaEffect<E> {
    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::new(ps).with_codec::<JsonCodec>().build();
    let store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let event_ref = EventPersistor::spawn(system.process_system(), store).await;
    let proxy = system
        .spawn_aggregate::<TestTarget>(AggregateId::new("test-tell-target"), event_ref)
        .await;
    SagaEffect::tell(proxy, NoopCmd)
}

// ---------------------------------------------------------------------------
// Shape — a PartialEq + Debug mirror of SagaEffect<E> used for structural
// comparison without requiring SagaEffect itself to implement PartialEq or
// Debug. The Tell variant does not carry data because the inner side effect
// is opaque.
// ---------------------------------------------------------------------------

#[derive(Debug, PartialEq)]
pub enum Shape<E> {
    None,
    Persist(Vec<E>),
    Tell,
    Sequence(Vec<Shape<E>>),
}

#[allow(dead_code)]
pub fn shape_of<E: Clone>(effect: &SagaEffect<E>) -> Shape<E> {
    match effect {
        SagaEffect::None => Shape::None,
        SagaEffect::Persist(events) => Shape::Persist(events.clone()),
        SagaEffect::Tell(_) => Shape::Tell,
        SagaEffect::Sequence(children) => {
            Shape::Sequence(children.iter().map(shape_of).collect())
        }
    }
}
