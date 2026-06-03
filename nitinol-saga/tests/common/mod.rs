use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use serde::{Deserialize, Serialize};

use nitinol_eventsource::codec::Codec;
use nitinol_eventsource::system::EventSourceSystem;
use nitinol_eventsource::{Aggregate, Context, Decider, Effect, Event};
use nitinol_persistence::store::{EventStore, InMemoryEventStore};
use nitinol_persistence::{AggregateId, EventType};
use nitinol_runtime::ProcessSystem;
use nitinol_saga::{SagaEffect, Schedule, TellIntent};

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

/// `NoopCmd` must implement `Clone` because the new ADT's `SagaEffect::tell`
/// keeps the command around for staged retries (each retry re-`tell`s the
/// target with a cloned copy).  `Serialize + Deserialize` is required because
/// `SagaEffect::tell` serializes the command as crash-restart payload.
#[derive(Clone, Serialize, Deserialize)]
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

/// Spin up a minimal system and return a `SagaEffect::tell(...)`.
///
/// Used by unit tests that need to exercise the tell-shaped effect (a
/// `Persist { events: [], tells: [_], schedules: [] }` value under the
/// post-#45 ADT) without triggering any real side effect.
#[allow(dead_code)]
pub async fn make_tell_effect<E>() -> SagaEffect<E> {
    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::new(ps).with_codec::<JsonCodec>().build();
    let store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let proxy = system
        .spawn_aggregate::<TestTarget>(AggregateId::new("test-tell-target"), store)
        .await;
    SagaEffect::tell(proxy, NoopCmd)
}

/// Build a single [`TellIntent`] over a freshly spawned no-op aggregate
/// target so tests can feed it into `SagaEffect::persist(...).with_tells(...)`
/// without going through the `SagaEffect::tell` convenience helper.
#[allow(dead_code)]
pub async fn make_tell_intent() -> TellIntent {
    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::new(ps).with_codec::<JsonCodec>().build();
    let store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let proxy = system
        .spawn_aggregate::<TestTarget>(AggregateId::new("test-tell-intent-target"), store)
        .await;
    TellIntent::new(proxy, NoopCmd)
}

// ---------------------------------------------------------------------------
// Shape — a PartialEq + Debug mirror of the post-#45 `SagaEffect<E>` ADT used
// for structural comparison without requiring `SagaEffect` itself to implement
// PartialEq or Debug.
//
// `TellIntent` is opaque (its inner side effect cannot be matched against), so
// `Persist` records only the *count* of tells.  `Schedule` carries a public
// `at` timestamp, so we capture that.
// ---------------------------------------------------------------------------

#[derive(Debug, PartialEq)]
#[allow(dead_code)]
pub enum Shape<E> {
    None,
    Persist {
        events: Vec<E>,
        tells: usize,
        schedules: Vec<jiff::Timestamp>,
    },
    End,
    Sequence(Vec<Shape<E>>),
}

#[allow(dead_code)]
pub fn shape_of<E: Clone>(effect: &SagaEffect<E>) -> Shape<E> {
    match effect {
        SagaEffect::None => Shape::None,
        SagaEffect::Persist {
            events,
            tells,
            schedules,
        } => Shape::Persist {
            events: events.clone(),
            tells: tells.len(),
            schedules: schedules.iter().map(schedule_at).collect(),
        },
        SagaEffect::End => Shape::End,
        SagaEffect::Sequence(children) => Shape::Sequence(children.iter().map(shape_of).collect()),
    }
}

#[allow(dead_code)]
pub fn schedule_at(schedule: &Schedule) -> jiff::Timestamp {
    schedule.at
}

/// Build a `Schedule` whose `at` field is `ts`.  Centralised so every test
/// uses the same construction path (and breaks together if `Schedule`'s
/// public shape ever changes).
#[allow(dead_code)]
pub fn schedule_at_ts(ts: jiff::Timestamp) -> Schedule {
    Schedule { at: ts }
}
