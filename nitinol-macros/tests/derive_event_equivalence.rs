//! Behavioral equivalence tests for `#[derive(Event)]`.
//!
//! The derive is pure sugar: its expansion must be
//! *functionally equivalent* to the hand-written `impl Event` pattern pinned in
//! `nitinol-eventsource/tests/event_variant.rs`. The hand-written naming rule is
//! "type name = type identifier, variant = arm identifier"; these tests assert
//! the derived `EVENT_TYPE` const and per-arm `variant()` values reproduce that
//! rule exactly.
//!
//! Both the `Event` trait and the `Event` derive are reached through the single
//! umbrella path `nitinol::eventsource::Event` (trait namespace vs. macro
//! namespace). This is the compatibility re-export path; the generated code
//! itself references the canonical `::nitinol::contract::Event` and
//! `::nitinol::persistence::{...}`.

use nitinol::eventsource::Event;
use nitinol::persistence::{EventType, Family, TypeName, Variant};

// struct: type-level identity (variant None), default variant() inherited

#[derive(Clone, Event)]
#[event(family = "shop.orders")]
struct Incremented;

/// Given a struct with `#[derive(Event)]` and an explicit family, When its
/// `EVENT_TYPE` is read, Then it equals the hand-written
/// `EventType::new(Family::new(family), TypeName::new(<type ident>))`.
#[test]
fn struct_event_type_matches_handwritten_const() {
    let expected = EventType::new(Family::new("shop.orders"), TypeName::new("Incremented"));

    assert_eq!(Incremented::EVENT_TYPE, expected);
    assert_eq!(Incremented::EVENT_TYPE.family(), Family::new("shop.orders"));
    assert_eq!(
        Incremented::EVENT_TYPE.type_name(),
        TypeName::new("Incremented")
    );
    assert_eq!(Incremented::EVENT_TYPE.variant(), None);
}

/// Given a struct event (no per-arm override), When `variant()` is called on a
/// value, Then it returns the type-level `EVENT_TYPE` (variant `None`),
/// matching the trait's default `variant()`.
#[test]
fn struct_value_variant_is_type_level_identity() {
    let event = Incremented;

    assert_eq!(event.variant(), Incremented::EVENT_TYPE);
    assert_eq!(event.variant().variant(), None);
}

// struct: empty family, mirroring `Family::new("")` in event_variant.rs

#[derive(Clone, Event)]
#[event(family = "")]
struct EmptyFamily;

/// Given a struct with an explicitly empty family, When `EVENT_TYPE` is read,
/// Then the family is the empty `Family` (not a fallback or default) and the
/// rendered form omits the family prefix.
#[test]
fn struct_empty_family_is_preserved_verbatim() {
    assert_eq!(EmptyFamily::EVENT_TYPE.family(), Family::new(""));
    assert_eq!(
        EmptyFamily::EVENT_TYPE.type_name(),
        TypeName::new("EmptyFamily")
    );
    assert_eq!(EmptyFamily::EVENT_TYPE.variant(), None);

    // Display omits the empty family prefix (persistence contract), proving the
    // derived const carries a genuinely empty family.
    assert_eq!(format!("{}", EmptyFamily::EVENT_TYPE), "EmptyFamily");
}

// enum: const is type-level (variant None); variant() fills the active arm

#[derive(Clone, Event)]
#[event(family = "saga.upstream")]
enum OrderEvent {
    Placed,
    Cancelled,
}

/// Given an enum with `#[derive(Event)]`, When its `EVENT_TYPE` const is read,
/// Then it is the type-level identity (variant `None`) named after the enum.
#[test]
fn enum_const_event_type_is_type_level() {
    let expected = EventType::new(Family::new("saga.upstream"), TypeName::new("OrderEvent"));

    assert_eq!(OrderEvent::EVENT_TYPE, expected);
    assert_eq!(OrderEvent::EVENT_TYPE.variant(), None);
}

/// Given an enum with `#[derive(Event)]`, When `variant()` is called on each
/// arm, Then it returns the arm-specific EventType whose variant equals the arm
/// identifier — identical to the hand-written match in event_variant.rs.
#[test]
fn enum_variant_returns_arm_identifier() {
    assert_eq!(
        OrderEvent::Placed.variant(),
        EventType::with_variant(
            Family::new("saga.upstream"),
            TypeName::new("OrderEvent"),
            Variant::new("Placed"),
        ),
    );
    assert_eq!(
        OrderEvent::Cancelled.variant(),
        EventType::with_variant(
            Family::new("saga.upstream"),
            TypeName::new("OrderEvent"),
            Variant::new("Cancelled"),
        ),
    );

    assert_eq!(
        OrderEvent::Placed.variant().variant(),
        Some(Variant::new("Placed"))
    );
    assert_eq!(
        OrderEvent::Cancelled.variant().variant(),
        Some(Variant::new("Cancelled"))
    );
}

/// Given an enum event whose arms carry `Some(variant)` identities, When
/// `type_key()` is taken, Then every arm shares the variant-free key of the
/// type-level `EVENT_TYPE` (so value-level events route to the type handler).
#[test]
fn enum_arms_share_type_key_with_const() {
    assert_eq!(
        OrderEvent::Placed.variant().type_key(),
        OrderEvent::EVENT_TYPE.type_key()
    );
    assert_eq!(
        OrderEvent::Cancelled.variant().type_key(),
        OrderEvent::EVENT_TYPE.type_key()
    );
}

// enum: unit / tuple / struct arms all match; arm fields are ignored

// Arm payloads exist only so the derive has tuple/struct shapes to expand over;
// the tests read `variant()`, never the field values.
#[allow(dead_code)]
#[derive(Clone, Event)]
#[event(family = "domain")]
enum Mixed {
    Unit,
    Tuple(u32, String),
    Struct { id: u64, name: String },
}

/// Given an enum mixing unit, tuple, and struct arms, When `variant()` is
/// called, Then every arm form matches and its variant is the arm identifier
/// (the derive must ignore payload fields via `(..)` / `{ .. }`).
#[test]
fn enum_variant_handles_all_arm_shapes() {
    let unit = Mixed::Unit;
    let tuple = Mixed::Tuple(7, String::from("x"));
    let structured = Mixed::Struct {
        id: 9,
        name: String::from("y"),
    };

    assert_eq!(unit.variant().variant(), Some(Variant::new("Unit")));
    assert_eq!(tuple.variant().variant(), Some(Variant::new("Tuple")));
    assert_eq!(structured.variant().variant(), Some(Variant::new("Struct")));

    assert_eq!(unit.variant().type_key(), Mixed::EVENT_TYPE.type_key());
    assert_eq!(tuple.variant().type_key(), Mixed::EVENT_TYPE.type_key());
    assert_eq!(
        structured.variant().type_key(),
        Mixed::EVENT_TYPE.type_key()
    );
}

// generics: split_for_impl passes type params / where-clause through

// `inner` exists only to give the type a parameter for `split_for_impl` to pass
// through; the tests read `EVENT_TYPE` and `variant()`, never the field value.
#[allow(dead_code)]
#[derive(Clone, Event)]
#[event(family = "generic")]
struct Wrapper<T: Clone + Send + Sync + 'static> {
    inner: T,
}

/// Given a generic struct that satisfies the `Event` supertrait bounds on its
/// type parameter, When the derive is applied, Then `EVENT_TYPE` uses the bare
/// type identifier (no generics in the name) and `variant()` works on a value.
#[test]
fn generic_struct_derive_uses_bare_type_name() {
    assert_eq!(
        <Wrapper<u32> as Event>::EVENT_TYPE,
        EventType::new(Family::new("generic"), TypeName::new("Wrapper")),
    );

    let value = Wrapper { inner: 1u32 };
    assert_eq!(value.variant(), <Wrapper<u32> as Event>::EVENT_TYPE);
    assert_eq!(value.variant().variant(), None);
}
