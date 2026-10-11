//! Shared test helpers. Each test binary compiles this module in full but
//! uses only the subset it needs, so per-binary dead code is expected.

#![allow(dead_code)]

use bytes::Bytes;
use serde::{Deserialize, Serialize};

use nitinol_eventsource::codec::Codec;
use nitinol_persistence::__private::reserved_family;
use nitinol_persistence::{EventType, Family, LoadedEvent, TypeName};
use nitinol_saga::{SagaEffect, SagaId, ScheduleSpec, TimerName};

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

// Shape — a PartialEq + Debug mirror of the `SagaEffect<E>` ADT used for
// structural comparison without requiring `SagaEffect` itself to implement
// PartialEq or Debug.
//
// `ScheduleSpec` is `PartialEq`, so we capture the specs verbatim.

#[derive(Debug, PartialEq)]
pub enum Shape<E> {
    None,
    Persist {
        events: Vec<E>,
        schedules: Vec<ScheduleSpec>,
    },
    End,
    Sequence(Vec<Shape<E>>),
    CancelSchedule,
}

pub fn shape_of<E: Clone>(effect: &SagaEffect<E>) -> Shape<E> {
    match effect {
        SagaEffect::None => Shape::None,
        SagaEffect::Persist { events, schedules } => Shape::Persist {
            events: events.clone(),
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

/// The one identity every schedule marker written on a saga's own stream is
/// written down as.  The transition (`scheduled` / `cancelled` / `fired`) is
/// carried by its payload's `oneof`, which [`schedule_kind_of`] reads.
pub const SCHEDULE_MARKER: EventType = EventType::new(SAGA_FAMILY, TypeName::new("schedule"));

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
    #[prost(string, tag = "6")]
    pub message_type: String,
    #[prost(string, tag = "7")]
    pub message_codec: String,
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
/// delay, the wall-clock anchor it is measured from, and the `message_type` /
/// `message_codec` identity the payload is named under.
pub fn encode_schedule_scheduled(
    saga_id: &SagaId,
    name: &TimerName,
    after: std::time::Duration,
    message_type: &str,
    message_codec: &str,
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
            message_type: message_type.to_owned(),
            message_codec: message_codec.to_owned(),
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

// Lifecycle facts — mirror of `proto/lifecycle.proto`.
//
// `LifecycleEvent` is `pub(crate)` in `nitinol-saga`, so an integration test
// reads the fact the way any other consumer of the column would: by decoding
// the payload's `oneof`, which is where the fact lives.

/// The one identity every lifecycle fact is written down as — a sibling of
/// [`SCHEDULE_MARKER`] and [`CONSUMED_MARKER`], so a saga's termination has a
/// declaration of its own rather than sharing an envelope with a transport
/// mechanism.
pub const LIFECYCLE_MARKER: EventType = EventType::new(SAGA_FAMILY, TypeName::new("lifecycle"));

/// Mirror of the production `Ended {}` lifecycle fact (proto tag 1).  The
/// message carries no fields — its presence in the saga stream is the durable
/// signal that the saga terminated.
#[derive(Clone, PartialEq, prost::Message)]
pub struct LifecycleEndedPayload {}

#[derive(Clone, PartialEq, prost::Oneof)]
pub enum LifecycleKind {
    #[prost(message, tag = "1")]
    Ended(LifecycleEndedPayload),
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct LifecycleMarkerPayload {
    #[prost(oneof = "LifecycleKind", tags = "1")]
    pub kind: Option<LifecycleKind>,
}

/// Encode the durable `Ended` lifecycle fact so tests can seed a "this saga
/// already terminated" stream before spawning the process.
pub fn encode_lifecycle_ended() -> Bytes {
    let msg = LifecycleMarkerPayload {
        kind: Some(LifecycleKind::Ended(LifecycleEndedPayload {})),
    };
    Bytes::from(prost::Message::encode_to_vec(&msg))
}

pub fn decode_lifecycle_kind(payload: &[u8]) -> LifecycleKind {
    <LifecycleMarkerPayload as prost::Message>::decode(payload)
        .expect("payload must be a valid prost LifecycleMarker message")
        .kind
        .expect("lifecycle marker payload must carry a oneof kind")
}

pub fn lifecycle_kind_of(event: &LoadedEvent) -> Option<LifecycleKind> {
    (event.event_type == LIFECYCLE_MARKER).then(|| decode_lifecycle_kind(&event.payload))
}

/// The fact a lifecycle row records, as the name the proto gives its `oneof`
/// arm.
pub fn lifecycle_fact(kind: &LifecycleKind) -> &'static str {
    match kind {
        LifecycleKind::Ended(_) => "ended",
    }
}

// Consumed facts — mirror of `proto/consumed.proto`.
//
// `ConsumedPositionEvent` is `pub(crate)` in `nitinol-saga`, so an integration
// test reads the fact the way any other consumer of the column would: by
// decoding the payload's `oneof`, which is where the fact lives.

/// The one identity every consumed fact is written down as — a sibling of
/// [`SCHEDULE_MARKER`] and [`LIFECYCLE_MARKER`], so a saga's input positions
/// have a declaration of their own.
///
/// Tests that count what a saga wrote select on this rather than adding one to
/// a literal: the row is framework bookkeeping, and an assertion that says
/// "exactly one user event" has to keep counting user events.
pub const CONSUMED_MARKER: EventType = EventType::new(SAGA_FAMILY, TypeName::new("consumed"));

#[derive(Clone, PartialEq, prost::Message)]
pub struct ConsumedFactPayload {
    /// Stream key of the upstream source the position belongs to.
    #[prost(string, tag = "1")]
    pub source: String,
    /// Highest upstream sequence of that source the saga has consumed.
    #[prost(uint64, tag = "2")]
    pub sequence: u64,
    /// Canonical path of the declared source's event type.  Two declared
    /// sources can live on one stream key, so the stream key alone does not
    /// name what a position settled.
    #[prost(string, tag = "3")]
    pub source_event_type: String,
}

#[derive(Clone, PartialEq, prost::Oneof)]
pub enum ConsumedKind {
    #[prost(message, tag = "1")]
    Consumed(ConsumedFactPayload),
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct ConsumedMarkerPayload {
    #[prost(oneof = "ConsumedKind", tags = "1")]
    pub kind: Option<ConsumedKind>,
}

pub fn encode_consumed_fact(source: &str, sequence: u64, source_event_type: &str) -> Bytes {
    let msg = ConsumedMarkerPayload {
        kind: Some(ConsumedKind::Consumed(ConsumedFactPayload {
            source: source.to_owned(),
            sequence,
            source_event_type: source_event_type.to_owned(),
        })),
    };
    Bytes::from(prost::Message::encode_to_vec(&msg))
}

pub fn decode_consumed_fact(payload: &[u8]) -> ConsumedFactPayload {
    let ConsumedKind::Consumed(consumed) =
        <ConsumedMarkerPayload as prost::Message>::decode(payload)
            .expect("payload must be a valid prost ConsumedMarker message")
            .kind
            .expect("consumed payload must carry a oneof kind");
    consumed
}

/// The fact a row holds, or `None` when the row is not one or cannot be read
/// as one.
///
/// Answers rather than insisting: a test whose subject *is* an unreadable row
/// has to be able to describe the stream it seeded without panicking on the
/// very row under test.
pub fn readable_consumed_fact_of(event: &LoadedEvent) -> Option<ConsumedFactPayload> {
    if event.event_type != CONSUMED_MARKER {
        return None;
    }
    <ConsumedMarkerPayload as prost::Message>::decode(&event.payload[..])
        .ok()?
        .kind
        .map(|ConsumedKind::Consumed(consumed)| consumed)
}
