//! What `#[derive(Event)]` places in the collection slice.
//!
//! ```gherkin
//!   Scenario: 手動登録なしで round-trip できる (the half that needs no database)
//!     Given #[derive(Event)] types — a struct, and an enum
//!     When a registry is built from what this binary linked
//!     Then each type's canonical path resolves, with nothing registered by hand
//!
//!   Scenario: one declaration, one path
//!     Given an enum whose arms carry different payloads
//!     Then the registry answers for the enum and for no path beneath it
//! ```
//!
//! The derive is still pure sugar for the hand-written `impl Event` — what it
//! writes alongside that impl is the entry a hand-written impl places for
//! itself. `derive_event_equivalence.rs` pins the impl; this file pins the
//! entry, and it is a file of its own because the slice is one per binary: what
//! the registry answers for here has to be exactly what this file derives.
//!
//! A row carries the path of the type that wrote it, so a type is written down
//! once. Which arm of an enum a row holds is the payload's to say, and the
//! codec that reads the payload is the one the declaration named.
//!
//! The generated entry is reached through the umbrella crate, as the rest of
//! the expansion is, so deriving `Event` obliges a user to depend on `nitinol`
//! and on nothing else.

use bytes::Bytes;
use nitinol::contract::Codec;
use nitinol::eventsource::Event;
use nitinol::persistence::EventRegistry;

/// The registry a binary that registered nothing by hand still has.
fn collected() -> EventRegistry {
    EventRegistry::from_linked()
        .expect("the event types this binary derives claim distinct canonical paths")
}

// struct with a family

#[derive(Clone, Event)]
#[event(family = "shop.registry", codec = OpenedCodec)]
struct Opened;

struct OpenedCodec;

impl Codec<Opened> for OpenedCodec {
    type Error = std::convert::Infallible;

    fn encode(_event: &Opened) -> Result<Bytes, Self::Error> {
        Ok(Bytes::new())
    }

    fn decode(_payload: &[u8]) -> Result<Opened, Self::Error> {
        Ok(Opened)
    }
}

// struct with no family: its canonical path is its type name alone

#[derive(Clone, Event)]
#[event(family = "", codec = UnfamiliedCodec)]
struct Unfamilied;

struct UnfamiliedCodec;

impl Codec<Unfamilied> for UnfamiliedCodec {
    type Error = std::convert::Infallible;

    fn encode(_event: &Unfamilied) -> Result<Bytes, Self::Error> {
        Ok(Bytes::new())
    }

    fn decode(_payload: &[u8]) -> Result<Unfamilied, Self::Error> {
        Ok(Unfamilied)
    }
}

// enum: one declaration, written down under one path.

// Arm payloads exist only so the derive has tuple/struct shapes to expand over;
// the tests read the declaration's path, never the field values.
#[allow(dead_code)]
#[derive(Clone, Event)]
#[event(family = "shop.registry", codec = SettledCodec)]
enum Settled {
    Paid { amount: u64 },
    Refunded(u64),
    Cancelled,
}

struct SettledCodec;

impl Codec<Settled> for SettledCodec {
    type Error = std::convert::Infallible;

    fn encode(_event: &Settled) -> Result<Bytes, Self::Error> {
        Ok(Bytes::new())
    }

    fn decode(_payload: &[u8]) -> Result<Settled, Self::Error> {
        Ok(Settled::Cancelled)
    }
}

#[test]
fn a_derived_struct_event_is_collected_under_its_canonical_path() {
    let registry = collected();

    assert_eq!(
        registry.resolve("shop.registry.Opened"),
        Some(Opened::EVENT_TYPE),
        "a derived event is served by the registry with nothing registered by hand, and it is \
         served as the very `EVENT_TYPE` the derive wrote"
    );
}

#[test]
fn a_derived_event_with_no_family_is_collected_under_its_type_name_alone() {
    let registry = collected();

    assert_eq!(
        registry.resolve("Unfamilied"),
        Some(Unfamilied::EVENT_TYPE),
        "an empty family is carried into the entry verbatim: the path collected is the path the \
         column holds, which for an empty family is the type name alone"
    );
}

#[test]
fn a_derived_enum_is_collected_once_under_the_path_its_rows_carry() {
    let registry = collected();

    assert_eq!(
        registry.resolve("shop.registry.Settled"),
        Some(Settled::EVENT_TYPE),
        "every row of this enum carries `shop.registry.Settled`, and that is the path the entry \
         has to answer for"
    );

    for arm in ["Paid", "Refunded", "Cancelled"] {
        assert_eq!(
            registry.resolve(&format!("shop.registry.Settled.{arm}")),
            None,
            "an arm is not a declaration: nothing writes `shop.registry.Settled.{arm}` into the \
             column, so nothing may answer for it either — a second entry there would be a second \
             identity for one type"
        );
    }
}

#[test]
fn a_type_the_binary_does_not_derive_is_not_collected() {
    let registry = collected();

    assert_eq!(
        registry.resolve("shop.registry.Withdrawn"),
        None,
        "the derive collects the types it expanded over, not every name in their family"
    );
}
