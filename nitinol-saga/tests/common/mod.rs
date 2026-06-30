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

// ---------------------------------------------------------------------------
// Prost mirrors of the framework outbox marker payloads (`nitinol.saga.outbox.*`).
//
// These mirror `nitinol-saga/proto/outbox.proto` exactly — same field numbers
// and wire types as the framework's `SystemEvent` codec:
//
//   message TellRequested { uint64 tell_id = 1; optional bytes crash_restart = 2; }
//   message TellAcked     { uint64 tell_id = 1; }
//   message TellFailed    { uint64 tell_id = 1; }
//   message Scheduled     { int64 at_unix_seconds = 1; }
//
// Integration tests live outside the saga crate and cannot reach the
// crate-internal prost types, so seeds and payload assertions go through these
// independent mirrors.  prost's wire format depends only on the field number +
// wire type, so bytes produced here decode in the framework and vice versa.
// ---------------------------------------------------------------------------

#[derive(Clone, PartialEq, prost::Message)]
#[allow(dead_code)]
pub struct TellRequestedPayload {
    #[prost(uint64, tag = "1")]
    pub tell_id: u64,
    #[prost(bytes = "vec", optional, tag = "2")]
    pub crash_restart: Option<Vec<u8>>,
}

#[derive(Clone, PartialEq, prost::Message)]
#[allow(dead_code)]
pub struct TellIdPayload {
    #[prost(uint64, tag = "1")]
    pub tell_id: u64,
}

#[derive(Clone, PartialEq, prost::Message)]
#[allow(dead_code)]
pub struct ScheduledPayload {
    #[prost(int64, tag = "1")]
    pub at_unix_seconds: i64,
}

/// Encode a `TellRequested` outbox payload exactly as the framework's prost
/// codec does: `tell_id` in field 1, optional `crash_restart` bytes in field 2.
/// `None` omits field 2 entirely (presence semantics for "no crash-restart").
#[allow(dead_code)]
pub fn encode_tell_requested(tell_id: u64, crash_restart: Option<&[u8]>) -> Bytes {
    let msg = TellRequestedPayload {
        tell_id,
        crash_restart: crash_restart.map(<[u8]>::to_vec),
    };
    Bytes::from(prost::Message::encode_to_vec(&msg))
}

/// Encode a terminal (`TellAcked` / `TellFailed`) outbox payload: just the
/// `tell_id` in field 1.
#[allow(dead_code)]
pub fn encode_tell_id(tell_id: u64) -> Bytes {
    let msg = TellIdPayload { tell_id };
    Bytes::from(prost::Message::encode_to_vec(&msg))
}

/// Encode a `Scheduled` outbox payload: the schedule time as whole unix seconds
/// in field 1 (mirrors `jiff::Timestamp::as_second`).
#[allow(dead_code)]
pub fn encode_scheduled_at(at: jiff::Timestamp) -> Bytes {
    let msg = ScheduledPayload {
        at_unix_seconds: at.as_second(),
    };
    Bytes::from(prost::Message::encode_to_vec(&msg))
}

/// Decode a `TellRequested` payload produced by the framework's prost codec.
#[allow(dead_code)]
pub fn decode_tell_requested(payload: &[u8]) -> TellRequestedPayload {
    <TellRequestedPayload as prost::Message>::decode(payload)
        .expect("payload must be a valid prost TellRequested message")
}

/// Decode a terminal (`TellAcked` / `TellFailed`) payload.
#[allow(dead_code)]
pub fn decode_tell_id(payload: &[u8]) -> TellIdPayload {
    <TellIdPayload as prost::Message>::decode(payload)
        .expect("payload must be a valid prost tell_id message")
}

/// Decode a `Scheduled` payload.
#[allow(dead_code)]
pub fn decode_scheduled(payload: &[u8]) -> ScheduledPayload {
    <ScheduledPayload as prost::Message>::decode(payload)
        .expect("payload must be a valid prost Scheduled message")
}
