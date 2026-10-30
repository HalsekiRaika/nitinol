//! Reading a stored `event_type` back as "is this the type that declared it?".
//!
//! A row's `event_type` column carries the identity its declaration stated, and
//! the question every consumer of a loaded row asks of it is whether that
//! identity is the one a given type answers for. The type owns the answer,
//! because the whole of what the answer rests on — the declaration's own
//! `EVENT_TYPE` — lives in the type.
//!
//! This file pins the question for the user-facing [`Event`] side. The
//! `SystemEvent` side is the same question asked of the framework's own
//! records, and is pinned in
//! `nitinol-saga/tests/saga_system_event_type_matching.rs`, where the types
//! that implement it live.

use bytes::Bytes;
use nitinol_eventsource::{codec::Codec, Event, MatchesEventType};
use nitinol_persistence::{EventType, Family, TypeName};

#[derive(Clone)]
struct OrderPlaced;

#[derive(Clone)]
struct OrderCancelled;

/// The codec both declarations name. These tests read identity alone, so the
/// format only has to exist.
struct UnitCodec;

impl Codec<OrderPlaced> for UnitCodec {
    type Error = std::convert::Infallible;

    fn encode(_: &OrderPlaced) -> Result<Bytes, Self::Error> {
        Ok(Bytes::new())
    }

    fn decode(_: &[u8]) -> Result<OrderPlaced, Self::Error> {
        Ok(OrderPlaced)
    }
}

impl Codec<OrderCancelled> for UnitCodec {
    type Error = std::convert::Infallible;

    fn encode(_: &OrderCancelled) -> Result<Bytes, Self::Error> {
        Ok(Bytes::new())
    }

    fn decode(_: &[u8]) -> Result<OrderCancelled, Self::Error> {
        Ok(OrderCancelled)
    }
}

impl Event for OrderPlaced {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("shop.order"), TypeName::new("OrderPlaced"));

    type Codec = UnitCodec;
}

impl Event for OrderCancelled {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("shop.order"), TypeName::new("OrderCancelled"));

    type Codec = UnitCodec;
}

/// Given the identity a stored row of `OrderPlaced` carries, written out the
/// way a store holds it rather than read off the declaration, When it is
/// matched against the `Event` that declared it, Then it answers yes.
#[test]
fn a_stored_event_type_matches_the_event_that_declares_it() {
    let stored = EventType::new(Family::new("shop.order"), TypeName::new("OrderPlaced"));

    assert!(
        stored.matches::<OrderPlaced>(),
        "`{stored}` is the identity `OrderPlaced` declares, so it must match it"
    );
}

/// Given that same stored identity, When it is matched against a sibling
/// declaration in the same family, Then it answers no: a declaration answers
/// for its own path alone, so two declarations never both claim one row.
#[test]
fn a_sibling_declaration_in_the_same_family_does_not_match() {
    let stored = EventType::new(Family::new("shop.order"), TypeName::new("OrderPlaced"));

    assert!(
        !stored.matches::<OrderCancelled>(),
        "`{stored}` must not match `OrderCancelled`: a sibling declares a path of its own"
    );
}

/// An identity is a family and a type name together, so a match is on both.
/// Either half agreeing on its own is a different declaration — which is what a
/// comparison written against one component would get wrong.
#[test]
fn matching_reads_the_whole_identity_rather_than_either_half() {
    let same_name_other_family =
        EventType::new(Family::new("shop.basket"), TypeName::new("OrderPlaced"));
    assert!(
        !same_name_other_family.matches::<OrderPlaced>(),
        "`{same_name_other_family}` shares only the type name, so it is a different declaration"
    );

    let same_family_other_name =
        EventType::new(Family::new("shop.order"), TypeName::new("OrderPlacedV2"));
    assert!(
        !same_family_other_name.matches::<OrderPlaced>(),
        "`{same_family_other_name}` shares only the family, so it is a different declaration"
    );
}
