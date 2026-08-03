//! Contract tests for the value-level `variant()` accessor added to `Event`
//! and `SystemEvent` (Issue #64).
//!
//! `Event` keeps a defaulted `fn variant(&self) -> EventType` returning
//! `Self::EVENT_TYPE`; `SystemEvent::variant()` is required (Issue #66) so an
//! enum implementor cannot silently drop its per-arm identity. A struct event
//! keeps the type-level identity (variant `None`) — via the `Event` default or
//! the explicit `Self::EVENT_TYPE` one-liner a `SystemEvent` struct writes.
//! These tests pin that type-level identity and the per-arm override.

use bytes::Bytes;
use nitinol_eventsource::{appending_system_event, Event, SystemEvent, SystemEventDecodeError};
use nitinol_persistence::{EventType, Family, TypeName, Variant};

// ---------------------------------------------------------------------------
// Event::variant default
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct Incremented;

impl Event for Incremented {
    const EVENT_TYPE: EventType = EventType::new(Family::new(""), TypeName::new("Incremented"));
}

/// Given a struct Event that does not override `variant`, When `variant()` is
/// called, Then it returns the declared `EVENT_TYPE`.
#[test]
fn event_default_variant_returns_event_type_const() {
    let event = Incremented;
    assert_eq!(event.variant(), Incremented::EVENT_TYPE);
    assert_eq!(event.variant().type_name(), TypeName::new("Incremented"));
    assert_eq!(event.variant().variant(), None);
}

/// Given an enum Event that overrides `variant` per arm, When `variant()` is
/// called, Then it returns the arm-specific EventType (proving the default is
/// overridable, not final).
#[derive(Clone)]
enum OrderEvent {
    Placed,
    Cancelled,
}

impl Event for OrderEvent {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("saga.upstream"), TypeName::new("OrderEvent"));

    fn variant(&self) -> EventType {
        let variant = match self {
            OrderEvent::Placed => Variant::new("Placed"),
            OrderEvent::Cancelled => Variant::new("Cancelled"),
        };
        EventType::with_variant(
            Family::new("saga.upstream"),
            TypeName::new("OrderEvent"),
            variant,
        )
    }
}

#[test]
fn event_overridden_variant_returns_arm_specific_event_type() {
    assert_eq!(
        OrderEvent::Placed.variant().variant(),
        Some(Variant::new("Placed"))
    );
    assert_eq!(
        OrderEvent::Cancelled.variant().variant(),
        Some(Variant::new("Cancelled"))
    );

    // Both arms share the same variant-free type-key as the const EVENT_TYPE.
    assert_eq!(
        OrderEvent::Placed.variant().type_key(),
        OrderEvent::EVENT_TYPE.type_key()
    );
    assert_eq!(
        OrderEvent::Cancelled.variant().type_key(),
        OrderEvent::EVENT_TYPE.type_key()
    );
}

// ---------------------------------------------------------------------------
// SystemEvent::variant default
// ---------------------------------------------------------------------------

struct Marker;

impl SystemEvent for Marker {
    const EVENT_TYPE: EventType = EventType::new(
        Family::new("nitinol.saga.outbox"),
        TypeName::new("scheduled"),
    );

    fn variant(&self) -> EventType {
        Self::EVENT_TYPE
    }

    fn encode(&self) -> Bytes {
        Bytes::new()
    }

    fn decode(_payload: &[u8]) -> Result<Self, SystemEventDecodeError> {
        Ok(Marker)
    }
}

/// Given a SystemEvent that does not override `variant`, When `variant()` is
/// called, Then it returns the declared `EVENT_TYPE` (consistent with `Event`).
#[test]
fn system_event_default_variant_returns_event_type_const() {
    let marker = Marker;
    assert_eq!(marker.variant(), Marker::EVENT_TYPE);
    assert_eq!(marker.variant().type_name(), TypeName::new("scheduled"));
    assert_eq!(marker.variant().variant(), None);
}

// ---------------------------------------------------------------------------
// Routing: variant-Some EventType reaches type-level handler via type_key()
// ---------------------------------------------------------------------------

/// Regression test for routing dispatch using type_key().
///
/// A LoadedEvent whose EventType carries a Some(Variant) arm identifier must
/// reach the handler registered for the type-level EVENT_TYPE (variant None).
/// The projection/process.rs and saga/process/props.rs dispatch paths use
/// `type_key()` comparison for exactly this reason.
#[test]
fn type_key_routing_matches_variant_some_to_type_level_handler() {
    // Simulate a variant-Some EventType arriving in the stream (e.g. from an
    // enum event's variant() override).
    let incoming = EventType::with_variant(
        Family::new("saga.upstream"),
        TypeName::new("OrderEvent"),
        Variant::new("Placed"),
    );

    // Simulate the type-level EventType registered for the handler.
    let registered = OrderEvent::EVENT_TYPE; // variant: None

    // Full Eq: variant Some != None → they differ.
    assert_ne!(incoming, registered);

    // type_key(): variant is ignored → they match, so the handler is invoked.
    assert_eq!(incoming.type_key(), registered.type_key());
}

// ---------------------------------------------------------------------------
// Regression: appending_system_event uses event.variant() not E::EVENT_TYPE
// ---------------------------------------------------------------------------

/// A minimal enum SystemEvent whose variant() overrides the type-level
/// EVENT_TYPE with an arm-specific Variant.  Used to verify that
/// `appending_system_event` stores `event.variant()` (Some) rather than
/// `E::EVENT_TYPE` (None).
enum MarkerEnum {
    Created,
}

impl SystemEvent for MarkerEnum {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("test.outbox"), TypeName::new("MarkerEnum"));

    fn variant(&self) -> EventType {
        let v = match self {
            MarkerEnum::Created => Variant::new("Created"),
        };
        EventType::with_variant(Family::new("test.outbox"), TypeName::new("MarkerEnum"), v)
    }

    fn encode(&self) -> Bytes {
        Bytes::new()
    }

    fn decode(_payload: &[u8]) -> Result<Self, SystemEventDecodeError> {
        Ok(MarkerEnum::Created)
    }
}

/// Regression test: `appending_system_event` must use `event.variant()` (not
/// `E::EVENT_TYPE`) so that enum `SystemEvent` implementors with arm-specific
/// `variant()` overrides have their variant stored in `AppendingEvent.event_type`.
///
/// Failure mode without the fix: the function used `E::EVENT_TYPE`, always
/// storing `variant=None` even when the event's `variant()` returns `Some(...)`.
#[test]
fn appending_system_event_stores_enum_variant_some() {
    let occurred_at = jiff::Timestamp::from_second(0).expect("epoch must be valid");
    let event = MarkerEnum::Created;

    let appending = appending_system_event(1, &event, occurred_at);

    assert_eq!(
        appending.event_type.variant(),
        Some(Variant::new("Created")),
        "appending_system_event must use event.variant() so enum SystemEvent's \
         arm variant is stored (Some), not the type-level EVENT_TYPE (None)"
    );
    assert_eq!(
        appending.event_type.type_key(),
        MarkerEnum::EVENT_TYPE.type_key(),
        "type_key must still match EVENT_TYPE for routing"
    );
}
