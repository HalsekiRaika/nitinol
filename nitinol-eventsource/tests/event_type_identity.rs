//! What identity a written event carries, for `Event` and for `SystemEvent`.
//!
//! An event is written down under the identity its declaration states, and only
//! that one. Which arm of an enum a value holds is carried by the payload — the
//! codec that declaration names is what reads it back — so the column names the
//! declaration and the payload names the case.
//!
//! This file pins the boundary both traits meet the store at: the identity
//! `appending_system_event` puts in `AppendingEvent.event_type`, and that the
//! arms of an enum are told apart by their bytes rather than by their path.
//!
//! The counterpart for the user-facing persist path is
//! `aggregate_process.rs::aggregate_persist_stores_the_declared_event_type_for_every_arm`.

use bytes::Bytes;
use nitinol_eventsource::{
    appending_system_event, codec::Codec, Event, SystemEvent, SystemEventDecodeError,
};
use nitinol_persistence::{EventType, Family, TypeName};

// Event — the declaration names the path

#[derive(Clone)]
struct Incremented;

/// The codec `Incremented` names. These tests read the identity, so the format
/// only has to exist.
struct UnitCodec;

impl Codec<Incremented> for UnitCodec {
    type Error = std::convert::Infallible;

    fn encode(_: &Incremented) -> Result<Bytes, Self::Error> {
        Ok(Bytes::new())
    }

    fn decode(_: &[u8]) -> Result<Incremented, Self::Error> {
        Ok(Incremented)
    }
}

impl Event for Incremented {
    const EVENT_TYPE: EventType = EventType::new(Family::new(""), TypeName::new("Incremented"));

    type Codec = UnitCodec;
}

/// An enum event whose arms are told apart by the bytes its codec writes.
#[derive(Clone, Debug, PartialEq)]
enum OrderEvent {
    Placed,
    Cancelled,
}

struct OrderEventCodec;

impl Codec<OrderEvent> for OrderEventCodec {
    type Error = std::convert::Infallible;

    fn encode(event: &OrderEvent) -> Result<Bytes, Self::Error> {
        Ok(Bytes::from_static(match event {
            OrderEvent::Placed => b"placed",
            OrderEvent::Cancelled => b"cancelled",
        }))
    }

    fn decode(payload: &[u8]) -> Result<OrderEvent, Self::Error> {
        Ok(if payload == b"cancelled" {
            OrderEvent::Cancelled
        } else {
            OrderEvent::Placed
        })
    }
}

impl Event for OrderEvent {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("saga.upstream"), TypeName::new("OrderEvent"));

    type Codec = OrderEventCodec;
}

/// Given an event declaration, When its identity is read, Then it is the family
/// and type name it was declared with, and its canonical path is the one string
/// a row of it carries.
#[test]
fn a_declaration_names_the_path_its_rows_carry() {
    assert_eq!(
        Incremented::EVENT_TYPE,
        EventType::new(Family::new(""), TypeName::new("Incremented"))
    );
    assert_eq!(Incremented::EVENT_TYPE.to_path().to_string(), "Incremented");

    assert_eq!(
        OrderEvent::EVENT_TYPE.to_path().to_string(),
        "saga.upstream.OrderEvent"
    );
}

/// Given the two arms of an enum event, When each is encoded, Then the bytes
/// differ and each decodes back to the arm it came from — the discriminant is
/// the payload's, which is why the identity does not need to carry it.
#[test]
fn the_arms_of_an_enum_event_are_told_apart_by_the_bytes_its_codec_writes() {
    let placed = OrderEventCodec::encode(&OrderEvent::Placed).expect("encode must succeed");
    let cancelled = OrderEventCodec::encode(&OrderEvent::Cancelled).expect("encode must succeed");

    assert_ne!(placed, cancelled);
    assert_eq!(
        OrderEventCodec::decode(&placed).expect("decode must succeed"),
        OrderEvent::Placed
    );
    assert_eq!(
        OrderEventCodec::decode(&cancelled).expect("decode must succeed"),
        OrderEvent::Cancelled
    );
}

// SystemEvent — the framework's own records reach the column the same way

struct Marker;

impl SystemEvent for Marker {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("test.outbox"), TypeName::new("Marker"));

    fn encode(&self) -> Bytes {
        Bytes::new()
    }

    fn decode(_payload: &[u8]) -> Result<Self, SystemEventDecodeError> {
        Ok(Marker)
    }
}

/// A minimal enum `SystemEvent`, shaped the way the saga markers are: one
/// reserved identity, and a leading byte that says which case the payload is.
enum MarkerEnum {
    Created,
    Retired,
}

const CREATED: u8 = 1;
const RETIRED: u8 = 2;

#[derive(Debug, thiserror::Error)]
#[error("a marker payload carries no case tag")]
struct UntaggedMarker;

impl SystemEvent for MarkerEnum {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("test.outbox"), TypeName::new("MarkerEnum"));

    fn encode(&self) -> Bytes {
        Bytes::from_static(match self {
            MarkerEnum::Created => &[CREATED],
            MarkerEnum::Retired => &[RETIRED],
        })
    }

    fn decode(payload: &[u8]) -> Result<Self, SystemEventDecodeError> {
        match payload.first() {
            Some(&CREATED) => Ok(MarkerEnum::Created),
            Some(&RETIRED) => Ok(MarkerEnum::Retired),
            _ => Err(SystemEventDecodeError::new(UntaggedMarker)),
        }
    }
}

/// Given a struct `SystemEvent`, When an `AppendingEvent` is built for it, Then
/// the row carries the declaration's identity.
#[test]
fn appending_a_system_event_stores_the_declared_identity() {
    let occurred_at = jiff::Timestamp::from_second(0).expect("epoch must be valid");

    let appending = appending_system_event(1, &Marker, occurred_at);

    assert_eq!(appending.event_type, Marker::EVENT_TYPE);
    assert_eq!(appending.sequence, 1);
}

/// Given an enum `SystemEvent`, When rows are built for two different cases,
/// Then both carry the one declared identity while their payloads differ — and
/// each payload decodes back to the case it was built from.
///
/// A framework record that wrote a per-case path would put a string in the
/// column that no declaration answers for, and the row would fail its load.
#[test]
fn every_case_of_an_enum_system_event_is_stored_under_the_one_declared_identity() {
    let occurred_at = jiff::Timestamp::from_second(0).expect("epoch must be valid");

    let created = appending_system_event(1, &MarkerEnum::Created, occurred_at);
    let retired = appending_system_event(2, &MarkerEnum::Retired, occurred_at);

    assert_eq!(created.event_type, MarkerEnum::EVENT_TYPE);
    assert_eq!(retired.event_type, MarkerEnum::EVENT_TYPE);

    assert_ne!(
        created.payload, retired.payload,
        "the case a row holds is carried by its bytes, so two cases cannot collapse to one payload"
    );
    assert!(matches!(
        MarkerEnum::decode(&created.payload),
        Ok(MarkerEnum::Created)
    ));
    assert!(matches!(
        MarkerEnum::decode(&retired.payload),
        Ok(MarkerEnum::Retired)
    ));
}
