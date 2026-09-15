//! How the provider turns a stored path back into an `EventType`.
//!
//! Every component of an `EventType` is a `&'static str`, so a value read out
//! of a column cannot become one without leaking the string it was read into.
//! The registry is the way out of that: a finite set of types the caller
//! declares, looked up by the canonical path the column holds.
//!
//! These clauses need no database, so they run wherever the crate is built.

use nitinol_persistence::{EventType, Family, TypeName, Variant};
use nitinol_sqlx::EventTypeRegistry;

const BARE: EventType = EventType::new(Family::new(""), TypeName::new("Bare"));
const NESTED: EventType =
    EventType::new(Family::new("sqlx.registry.deep"), TypeName::new("Nested"));
const ARM: EventType = EventType::with_variant(
    Family::new("sqlx.registry"),
    TypeName::new("Choice"),
    Variant::new("Left"),
);

#[test]
fn a_registered_event_type_resolves_from_the_path_it_is_stored_under() {
    let mut registry = EventTypeRegistry::new();
    registry.register(BARE);
    registry.register(NESTED);
    registry.register(ARM);

    assert_eq!(
        registry.resolve("Bare"),
        Some(BARE),
        "an event type with no family is stored under its type name alone"
    );
    assert_eq!(
        registry.resolve("sqlx.registry.deep.Nested"),
        Some(NESTED),
        "an event type with a family is stored under family and type name"
    );
    assert_eq!(
        registry.resolve("sqlx.registry.Choice.Left"),
        Some(ARM),
        "an enum arm is stored under the arm's path, variant included"
    );
}

#[test]
fn a_path_no_one_registered_resolves_to_nothing() {
    let mut registry = EventTypeRegistry::new();
    registry.register(NESTED);

    assert_eq!(
        registry.resolve("sqlx.registry.deep.Unregistered"),
        None,
        "a registry answers for the types it was given and no others"
    );
    assert_eq!(
        registry.resolve(""),
        None,
        "the empty path is no type's canonical path"
    );
}

#[test]
fn registering_a_second_type_under_one_path_reports_the_one_it_replaced() {
    // One canonical path, two readings of it: a struct event named `C` in
    // family `a.b`, and arm `C` of type `b` in family `a`. A caller that
    // registers both would otherwise lose one without being told.
    let struct_event = EventType::new(Family::new("a.b"), TypeName::new("C"));
    let arm_event =
        EventType::with_variant(Family::new("a"), TypeName::new("b"), Variant::new("C"));
    assert_eq!(
        struct_event.to_path().to_string(),
        arm_event.to_path().to_string(),
        "the two readings share one canonical path, which is what makes the collision possible"
    );

    let mut registry = EventTypeRegistry::new();

    assert_eq!(
        registry.register(struct_event),
        None,
        "the first registration of a path replaces nothing"
    );
    assert_eq!(
        registry.register(arm_event),
        Some(struct_event),
        "a registration that takes a path over reports the type it displaced"
    );
    assert_eq!(
        registry.resolve("a.b.C"),
        Some(arm_event),
        "the last registration of a path is the one that answers for it"
    );
}

#[test]
fn a_registry_collected_from_event_types_resolves_every_one_of_them() {
    let registry: EventTypeRegistry = [BARE, NESTED, ARM].into_iter().collect();

    assert_eq!(registry.resolve("Bare"), Some(BARE));
    assert_eq!(registry.resolve("sqlx.registry.deep.Nested"), Some(NESTED));
    assert_eq!(registry.resolve("sqlx.registry.Choice.Left"), Some(ARM));
}
