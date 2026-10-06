//! Shared test helpers. Each test binary compiles this module in full but
//! uses only the subset it needs, so per-binary dead code is expected.

#![allow(dead_code)]

use std::sync::Arc;

use bytes::Bytes;
use serde::{Deserialize, Serialize};

use nitinol_eventsource::codec::Codec;
use nitinol_eventsource::system::EventSourceSystem;
use nitinol_eventsource::{Aggregate, Decider, Decision, Event, SystemEvent};
use nitinol_persistence::__private::reserved_family;
use nitinol_persistence::provider::Persistence;
use nitinol_persistence::store::{EventStore, InMemoryEventStore};
use nitinol_persistence::{AggregateId, EventType, Family, LoadedEvent, TypeName};
use nitinol_runtime::ProcessSystem;
use nitinol_saga::{
    DeadLetterEvent, SagaEffect, SagaFailure, SagaId, ScheduleSpec, TellIntent, TimerName,
};

// JsonCodec — shared across all integration tests

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

// A saga's own time needs no helper: the host resolves the one resident
// `SchedulerProcess` from the saga's alphabet, so a test that wants deadlines
// declares a `#[subscribe(deadline)]` variant and asks the components for a
// `Deadline<M>`. There is no proxy for a test to hold, which is the point —
// holding one would make "arm a timer without writing the fact" expressible.

// Minimal aggregate used only to obtain an AggregateProxy for Tell tests.

#[derive(Default)]
pub struct TestTarget;

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct TestTargetEvent;

impl Event for TestTargetEvent {
    const EVENT_TYPE: EventType = EventType::new(Family::new("test"), TypeName::new("noop_target"));
    type Codec = JsonCodec;
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
pub struct NoopCmd;

impl Decider<NoopCmd> for TestTarget {
    type Output = ();
    type Rejection = std::convert::Infallible;

    fn decide(&self, _cmd: NoopCmd) -> Decision<TestTargetEvent, (), Self::Rejection> {
        Decision::persist(Vec::new()).output(())
    }
}

/// Spin up a minimal system and return a `SagaEffect::tell(...)`.
///
/// Used by unit tests that need to exercise the tell-shaped effect (a
/// `Persist { events: [], tells: [_], schedules: [] }` value) without
/// triggering any real side effect.
pub async fn make_tell_effect<E>() -> SagaEffect<E> {
    let ps = ProcessSystem::new().await;
    let store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let system = EventSourceSystem::builder(ps)
        .with_persistence(Persistence::new(store))
        .build();
    let proxy = system
        .spawn_aggregate::<TestTarget>(AggregateId::new("test-tell-target"))
        .await;
    SagaEffect::tell(proxy, NoopCmd)
}

/// Build a single [`TellIntent`] over a freshly spawned no-op aggregate
/// target so tests can feed it into `SagaEffect::tell_intent(...)` without
/// going through the `SagaEffect::tell` convenience helper.
pub async fn make_tell_intent() -> TellIntent {
    let ps = ProcessSystem::new().await;
    let store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let system = EventSourceSystem::builder(ps)
        .with_persistence(Persistence::new(store))
        .build();
    let proxy = system
        .spawn_aggregate::<TestTarget>(AggregateId::new("test-tell-intent-target"))
        .await;
    TellIntent::new(proxy, NoopCmd)
}

// Shape — a PartialEq + Debug mirror of the `SagaEffect<E>` ADT used for
// structural comparison without requiring `SagaEffect` itself to implement
// PartialEq or Debug.
//
// `TellIntent` is opaque (its inner side effect cannot be matched against), so
// `Persist` records only the *count* of tells.  `ScheduleSpec` is `PartialEq`,
// so we capture the specs verbatim.

#[derive(Debug, PartialEq)]
pub enum Shape<E> {
    None,
    Persist {
        events: Vec<E>,
        tells: usize,
        schedules: Vec<ScheduleSpec>,
    },
    End,
    Sequence(Vec<Shape<E>>),
    CancelSchedule,
}

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
            schedules: schedules.clone(),
        },
        SagaEffect::End => Shape::End,
        SagaEffect::Sequence(children) => Shape::Sequence(children.iter().map(shape_of).collect()),
        SagaEffect::CancelSchedule(_) => Shape::CancelSchedule,
    }
}

/// The framework's own family. `Family::new` refuses the reserved namespace, so
/// a test that has to name a framework record — to find it on a stream it has
/// loaded — goes through the same constructor the framework itself does.
const SAGA_FAMILY: Family = reserved_family("nitinol.saga");

/// The one identity every outbox marker is written down as.  Which transition a
/// marker records (`tell_requested` / `tell_acked` / `tell_failed` /
/// `scheduled` / `ended`) is carried by its payload's `oneof`, which
/// [`outbox_kind_of`] reads.
pub const OUTBOX_MARKER: EventType = EventType::new(SAGA_FAMILY, TypeName::new("outbox"));

/// The one identity every schedule marker written on a saga's own stream is
/// written down as.  The transition (`scheduled` / `cancelled` / `fired`) is
/// carried by its payload's `oneof`, which [`schedule_kind_of`] reads.
pub const SCHEDULE_MARKER: EventType = EventType::new(SAGA_FAMILY, TypeName::new("schedule"));

/// The one identity every dead letter is written down as.
pub const DEAD_LETTER_MARKER: EventType = EventType::new(SAGA_FAMILY, TypeName::new("dead_letter"));

/// The one identity every disposition marker is written down as — a sibling of
/// [`DEAD_LETTER_MARKER`], so selecting dead letters never selects settlements.
pub const DISPOSITION_MARKER: EventType =
    EventType::new(SAGA_FAMILY, TypeName::new("dead_letter_disposition"));

/// The one identity every consumed-position record is written down as — a
/// sibling of [`OUTBOX_MARKER`], so counting outbox markers never counts the
/// input positions the framework folds its de-duplication out of.
///
/// Tests that count what a saga wrote select on this rather than adding one to
/// a literal: the row is framework bookkeeping, and an assertion that says
/// "exactly one user event" has to keep counting user events.
pub const CONSUMED_POSITION_MARKER: EventType =
    EventType::new(SAGA_FAMILY, TypeName::new("consumed_position"));

#[derive(Clone, PartialEq, prost::Message)]
pub struct TellRequestedPayload {
    #[prost(uint64, tag = "1")]
    pub tell_id: u64,
    #[prost(bytes = "vec", optional, tag = "2")]
    pub crash_restart: Option<Vec<u8>>,
    /// Target aggregate stream key (field 3, added with DLQ replay fix).
    /// Empty string for streams written before this field was added.
    #[prost(string, tag = "3")]
    pub target: String,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct TellIdPayload {
    #[prost(uint64, tag = "1")]
    pub tell_id: u64,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct ScheduledPayload {
    #[prost(int64, tag = "1")]
    pub at_unix_seconds: i64,
}

/// Mirror of the production `Ended {}` outbox marker (proto tag 5).  The
/// message carries no fields — its presence in the saga stream is the durable
/// signal that the saga terminated.
#[derive(Clone, PartialEq, prost::Message)]
pub struct EndedPayload {}

#[derive(Clone, PartialEq, prost::Oneof)]
pub enum OutboxKind {
    #[prost(message, tag = "1")]
    TellRequested(TellRequestedPayload),
    #[prost(message, tag = "2")]
    TellAcked(TellIdPayload),
    #[prost(message, tag = "3")]
    TellFailed(TellIdPayload),
    #[prost(message, tag = "4")]
    Scheduled(ScheduledPayload),
    #[prost(message, tag = "5")]
    Ended(EndedPayload),
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct OutboxMarkerPayload {
    #[prost(oneof = "OutboxKind", tags = "1, 2, 3, 4, 5")]
    pub kind: Option<OutboxKind>,
}

pub fn encode_outbox_marker(kind: OutboxKind) -> Bytes {
    let msg = OutboxMarkerPayload { kind: Some(kind) };
    Bytes::from(prost::Message::encode_to_vec(&msg))
}

pub fn encode_outbox_tell_requested(tell_id: u64, crash_restart: Option<&[u8]>) -> Bytes {
    encode_outbox_marker(OutboxKind::TellRequested(TellRequestedPayload {
        tell_id,
        crash_restart: crash_restart.map(<[u8]>::to_vec),
        target: String::new(),
    }))
}

/// Encode a `TellRequested` outbox marker with an explicit `target` stream key
/// (field 3).  Used by tests that exercise the DLQ replay path, which requires
/// the target to be stored in the durable `TellRequested` record.
pub fn encode_outbox_tell_requested_with_target(
    tell_id: u64,
    crash_restart: Option<&[u8]>,
    target: &str,
) -> Bytes {
    encode_outbox_marker(OutboxKind::TellRequested(TellRequestedPayload {
        tell_id,
        crash_restart: crash_restart.map(<[u8]>::to_vec),
        target: target.to_owned(),
    }))
}

pub fn encode_outbox_tell_acked(tell_id: u64) -> Bytes {
    encode_outbox_marker(OutboxKind::TellAcked(TellIdPayload { tell_id }))
}

pub fn encode_outbox_tell_failed(tell_id: u64) -> Bytes {
    encode_outbox_marker(OutboxKind::TellFailed(TellIdPayload { tell_id }))
}

/// Encode the durable `Ended` terminal marker (proto tag 5) so tests can
/// seed a "this saga already terminated" stream before spawning the process.
pub fn encode_outbox_ended() -> Bytes {
    encode_outbox_marker(OutboxKind::Ended(EndedPayload {}))
}

pub fn decode_outbox_kind(payload: &[u8]) -> OutboxKind {
    <OutboxMarkerPayload as prost::Message>::decode(payload)
        .expect("payload must be a valid prost OutboxMarker message")
        .kind
        .expect("outbox marker payload must carry a oneof kind")
}

pub fn outbox_kind_of(event: &LoadedEvent) -> Option<OutboxKind> {
    (event.event_type == OUTBOX_MARKER).then(|| decode_outbox_kind(&event.payload))
}

// Schedule markers — mirror of `proto/schedule.proto`.
//
// `ScheduleEvent` is `pub(crate)` in `nitinol-saga`, so an integration test
// reads the transition the way any other consumer of the column would: by
// decoding the payload's `oneof`, which is where the transition lives.

#[derive(Clone, PartialEq, prost::Message)]
pub struct ScheduledMarkerPayload {
    #[prost(string, tag = "1")]
    pub saga_id: String,
    #[prost(string, tag = "2")]
    pub name: String,
    #[prost(uint64, tag = "3")]
    pub after_nanos: u64,
    #[prost(bytes = "vec", tag = "4")]
    pub payload: Vec<u8>,
    #[prost(int64, tag = "5")]
    pub scheduled_at_unix_millis: i64,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct CancelledMarkerPayload {
    #[prost(string, tag = "1")]
    pub saga_id: String,
    #[prost(string, tag = "2")]
    pub name: String,
    #[prost(int64, tag = "3")]
    pub cancelled_at_unix_millis: i64,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct FiredMarkerPayload {
    #[prost(string, tag = "1")]
    pub saga_id: String,
    #[prost(string, tag = "2")]
    pub name: String,
    #[prost(int64, tag = "3")]
    pub fired_at_unix_millis: i64,
}

#[derive(Clone, PartialEq, prost::Oneof)]
pub enum ScheduleKind {
    #[prost(message, tag = "1")]
    Scheduled(ScheduledMarkerPayload),
    #[prost(message, tag = "2")]
    Cancelled(CancelledMarkerPayload),
    #[prost(message, tag = "3")]
    Fired(FiredMarkerPayload),
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct ScheduleMarkerPayload {
    #[prost(oneof = "ScheduleKind", tags = "1, 2, 3")]
    pub kind: Option<ScheduleKind>,
}

/// Encode the `scheduled` arm so a test can seed a stream that already holds an
/// unfired schedule — the state a crash leaves behind — without first running
/// an incarnation that would also arm the timer in a live scheduler.
///
/// Field for field this is what `ScheduleEvent::Scheduled` writes: the nanos
/// delay and the wall-clock anchor it is measured from.
pub fn encode_schedule_scheduled(
    saga_id: &SagaId,
    name: &TimerName,
    after: std::time::Duration,
    payload: &[u8],
    scheduled_at_unix_millis: i64,
) -> Bytes {
    let msg = ScheduleMarkerPayload {
        kind: Some(ScheduleKind::Scheduled(ScheduledMarkerPayload {
            saga_id: saga_id.as_str().to_owned(),
            name: name.as_str().to_owned(),
            after_nanos: u64::try_from(after.as_nanos()).unwrap_or(u64::MAX),
            payload: payload.to_vec(),
            scheduled_at_unix_millis,
        })),
    };
    Bytes::from(prost::Message::encode_to_vec(&msg))
}

pub fn decode_schedule_kind(payload: &[u8]) -> ScheduleKind {
    <ScheduleMarkerPayload as prost::Message>::decode(payload)
        .expect("payload must be a valid prost ScheduleMarker message")
        .kind
        .expect("schedule marker payload must carry a oneof kind")
}

pub fn schedule_kind_of(event: &LoadedEvent) -> Option<ScheduleKind> {
    (event.event_type == SCHEDULE_MARKER).then(|| decode_schedule_kind(&event.payload))
}

/// The transition a schedule marker records, as the name the proto gives it.
///
/// Test assertions read as the wire does; the string is the `oneof` field name,
/// not a path segment.
pub fn schedule_transition(kind: &ScheduleKind) -> &'static str {
    match kind {
        ScheduleKind::Scheduled(_) => "scheduled",
        ScheduleKind::Cancelled(_) => "cancelled",
        ScheduleKind::Fired(_) => "fired",
    }
}

// Consumed-position records — mirror of `proto/consumed_position.proto`.
//
// `ConsumedPositionEvent` is `pub(crate)` in `nitinol-saga`, so an integration
// test reads the fact the way any other consumer of the column would: by
// decoding the payload's `oneof`, which is where the fact lives.

#[derive(Clone, PartialEq, prost::Message)]
pub struct ConsumedPayload {
    /// Stream key of the upstream source the position belongs to.
    #[prost(string, tag = "1")]
    pub source: String,
    /// Highest upstream sequence of that source the saga has consumed.
    #[prost(uint64, tag = "2")]
    pub sequence: u64,
}

#[derive(Clone, PartialEq, prost::Oneof)]
pub enum ConsumedPositionKind {
    #[prost(message, tag = "1")]
    Consumed(ConsumedPayload),
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct ConsumedPositionMarkerPayload {
    #[prost(oneof = "ConsumedPositionKind", tags = "1")]
    pub kind: Option<ConsumedPositionKind>,
}

pub fn encode_consumed_position(source: &str, sequence: u64) -> Bytes {
    let msg = ConsumedPositionMarkerPayload {
        kind: Some(ConsumedPositionKind::Consumed(ConsumedPayload {
            source: source.to_owned(),
            sequence,
        })),
    };
    Bytes::from(prost::Message::encode_to_vec(&msg))
}

pub fn decode_consumed_position(payload: &[u8]) -> ConsumedPayload {
    let ConsumedPositionKind::Consumed(consumed) =
        <ConsumedPositionMarkerPayload as prost::Message>::decode(payload)
            .expect("payload must be a valid prost ConsumedPositionMarker message")
            .kind
            .expect("consumed position payload must carry a oneof kind");
    consumed
}

/// The position a row holds, or `None` when the row is not one or cannot be
/// read as one.
///
/// Unlike [`decode_consumed_position`], this answers rather than insisting: a
/// test whose subject *is* an unreadable row has to be able to describe the
/// stream it seeded without panicking on the very row under test.
pub fn readable_consumed_position_of(event: &LoadedEvent) -> Option<ConsumedPayload> {
    if event.event_type != CONSUMED_POSITION_MARKER {
        return None;
    }
    <ConsumedPositionMarkerPayload as prost::Message>::decode(&event.payload[..])
        .ok()?
        .kind
        .map(|ConsumedPositionKind::Consumed(consumed)| consumed)
}

// Disposition markers — mirror of `proto/dead_letter_disposition.proto`.

#[derive(Clone, PartialEq, prost::Message)]
pub struct ProcessedPayload {}

#[derive(Clone, PartialEq, prost::Message)]
pub struct EvictedPayload {}

#[derive(Clone, PartialEq, prost::Oneof)]
pub enum DispositionKind {
    #[prost(message, tag = "2")]
    Processed(ProcessedPayload),
    #[prost(message, tag = "3")]
    Evicted(EvictedPayload),
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct DispositionMarkerPayload {
    #[prost(uint64, tag = "1")]
    pub dead_letter_sequence: u64,
    #[prost(oneof = "DispositionKind", tags = "2, 3")]
    pub disposition: Option<DispositionKind>,
}

pub fn decode_disposition(payload: &[u8]) -> DispositionMarkerPayload {
    <DispositionMarkerPayload as prost::Message>::decode(payload)
        .expect("payload must be a valid prost DeadLetterDispositionMarker message")
}

pub fn disposition_of(event: &LoadedEvent) -> Option<DispositionMarkerPayload> {
    (event.event_type == DISPOSITION_MARKER).then(|| decode_disposition(&event.payload))
}

/// The settlement a disposition marker records, as the name the proto gives it.
pub fn disposition_kind(marker: &DispositionMarkerPayload) -> &'static str {
    match marker
        .disposition
        .as_ref()
        .expect("a disposition marker carries a oneof disposition")
    {
        DispositionKind::Processed(_) => "processed",
        DispositionKind::Evicted(_) => "evicted",
    }
}

// Dead letters — read through the public record type.
//
// `DeadLetterEvent` is public, so a test reads the failure a record holds the
// way any consumer does: it decodes the payload. The identity column names the
// declaration; which failure it is belongs to the bytes.

pub fn decode_dead_letter(payload: &[u8]) -> DeadLetterEvent {
    <DeadLetterEvent as SystemEvent>::decode(payload)
        .expect("payload must be a valid dead letter record")
}

pub fn dead_letter_of(event: &LoadedEvent) -> Option<DeadLetterEvent> {
    (event.event_type == DEAD_LETTER_MARKER).then(|| decode_dead_letter(&event.payload))
}

/// The failure a dead letter records, as the name the proto gives its `oneof`
/// arm — the same names the DLQ documentation uses.
pub fn failure_kind(failure: &SagaFailure) -> &'static str {
    match failure {
        SagaFailure::TellFailed { .. } => "tell_failed",
        SagaFailure::HandleFailed { .. } => "handle_failed",
        SagaFailure::PersistFailed { .. } => "persist_failed",
        SagaFailure::EndedSagaReceivedMessage { .. } => "ended_saga_received_message",
        SagaFailure::DecodeFailed { .. } => "decode_failed",
        SagaFailure::ScheduledFailed { .. } => "scheduled_failed",
        SagaFailure::TellFailedHookFailed { .. } => "tell_failed_hook_failed",
    }
}

/// Does `payload` record a failure of the named kind?
///
/// Takes the bytes rather than a loaded row so the write path — a store that
/// inspects the `AppendingEvent` it was handed — can ask the same question.
pub fn dead_letter_records(payload: &[u8], kind: &str) -> bool {
    failure_kind(&decode_dead_letter(payload).failure) == kind
}
