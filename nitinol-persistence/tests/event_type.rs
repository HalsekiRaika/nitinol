//! Contract tests for the restructured [`EventType`] (Issues #64, #66).
//!
//! `EventType` is redefined from a single `&'static str` into three components
//! — `Family` / `TypeName` / `Variant` — with structured equality, a
//! variant-free type-key, hierarchical family matching, and a `Display` impl
//! that renders the one-way canonical `family.TypeName[.variant]` path.
//! These tests pin the public contract the framework and future tooling rely on.
//!
//! # Round-trip via `MaterializedPath`
//!
//! `EventType` uses `&'static str` for all components and is therefore `Copy`
//! and `const`-constructable, but cannot implement `FromStr` (runtime strings
//! cannot be borrowed as `&'static str`).  The parse boundary is
//! [`MaterializedPath`]: `event_type.to_path().to_string()` produces the
//! canonical wire string, which `str::parse::<MaterializedPath>()` recovers
//! without loss.  See [`event_type_round_trips_as_materialized_path`] for the
//! pinned contract test.

use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};

use nitinol_persistence::{
    EventType, Family, MaterializedPath, ParsedEventType, TypeName, Variant,
};

fn hash_of<T: Hash>(value: &T) -> u64 {
    let mut hasher = DefaultHasher::new();
    value.hash(&mut hasher);
    hasher.finish()
}

// ---------------------------------------------------------------------------
// Component newtypes: Family / TypeName / Variant
// ---------------------------------------------------------------------------

/// Given a Family created from a str, When as_str is read, Then the original
/// str is returned unchanged.
#[test]
fn family_as_str_round_trips_the_source_string() {
    let family = Family::new("nitinol.saga.outbox");
    assert_eq!(family.as_str(), "nitinol.saga.outbox");
}

/// Given a TypeName created from a str, When as_str is read, Then the original
/// str is returned unchanged.
#[test]
fn type_name_as_str_round_trips_the_source_string() {
    let type_name = TypeName::new("tell_requested");
    assert_eq!(type_name.as_str(), "tell_requested");
}

/// Given a Variant created from a str, When as_str is read, Then the original
/// str is returned unchanged.
#[test]
fn variant_as_str_round_trips_the_source_string() {
    let variant = Variant::new("Reserved");
    assert_eq!(variant.as_str(), "Reserved");
}

// ---------------------------------------------------------------------------
// Family::is_within — hierarchical prefix match on segment boundaries
// ---------------------------------------------------------------------------

/// Given a descendant family, When compared against an ancestor prefix, Then it
/// is reported as within that ancestor.
#[test]
fn family_is_within_true_for_descendant_on_segment_boundary() {
    let child = Family::new("nitinol.saga.outbox");
    let ancestor = Family::new("nitinol.saga");
    assert!(child.is_within(ancestor));
}

/// Given a family equal to the ancestor, When compared, Then it is reported as
/// within (a family contains itself).
#[test]
fn family_is_within_true_for_exact_match() {
    let family = Family::new("nitinol.saga");
    assert!(family.is_within(family));
}

/// Given a family that shares a raw string prefix but crosses no segment
/// boundary, When compared, Then it is reported as NOT within (`nitinol.sagax`
/// is not under `nitinol.saga`).
#[test]
fn family_is_within_false_for_raw_prefix_without_segment_boundary() {
    let sibling = Family::new("nitinol.sagax");
    let ancestor = Family::new("nitinol.saga");
    assert!(!sibling.is_within(ancestor));
}

/// Given an ancestor deeper than the family, When compared, Then the shallower
/// family is NOT within the deeper one (direction matters).
#[test]
fn family_is_within_false_when_ancestor_is_deeper() {
    let shallow = Family::new("nitinol.saga");
    let deeper = Family::new("nitinol.saga.outbox");
    assert!(!shallow.is_within(deeper));
}

/// Given two unrelated families, When compared, Then neither is within the
/// other.
#[test]
fn family_is_within_false_for_unrelated_families() {
    let a = Family::new("nitinol.saga");
    let b = Family::new("nitinol.projection");
    assert!(!a.is_within(b));
}

// ---------------------------------------------------------------------------
// EventType construction and accessors
// ---------------------------------------------------------------------------

/// Given an EventType built with `new`, When its components are read, Then
/// family/type_name match and variant is None (struct events).
#[test]
fn new_sets_components_and_leaves_variant_none() {
    let et = EventType::new(
        Family::new("nitinol.saga.outbox"),
        TypeName::new("tell_requested"),
    );

    assert_eq!(et.family(), Family::new("nitinol.saga.outbox"));
    assert_eq!(et.type_name(), TypeName::new("tell_requested"));
    assert_eq!(et.variant(), None);
}

/// Given an EventType built with `with_variant`, When its components are read,
/// Then variant is Some with the supplied value (enum arm events).
#[test]
fn with_variant_sets_variant_some() {
    let et = EventType::with_variant(
        Family::new("saga.upstream"),
        TypeName::new("OrderEvent"),
        Variant::new("OrderPlaced"),
    );

    assert_eq!(et.family(), Family::new("saga.upstream"));
    assert_eq!(et.type_name(), TypeName::new("OrderEvent"));
    assert_eq!(et.variant(), Some(Variant::new("OrderPlaced")));
}

// ---------------------------------------------------------------------------
// type_key — variant-free identity for decode routing / registry keys
// ---------------------------------------------------------------------------

/// Given two EventTypes sharing family+type_name but differing in variant
/// (Some vs None), When their type-keys are compared, Then the keys are equal
/// (variant is excluded from the key).
#[test]
fn type_key_ignores_variant_for_some_vs_none() {
    let without = EventType::new(Family::new("saga.upstream"), TypeName::new("OrderEvent"));
    let with = EventType::with_variant(
        Family::new("saga.upstream"),
        TypeName::new("OrderEvent"),
        Variant::new("OrderPlaced"),
    );

    assert_eq!(without.type_key(), with.type_key());
}

/// Given two EventTypes sharing family+type_name but differing in their Some
/// variant, When their type-keys are compared, Then the keys are equal.
#[test]
fn type_key_ignores_differing_some_variants() {
    let placed = EventType::with_variant(
        Family::new("saga.upstream"),
        TypeName::new("OrderEvent"),
        Variant::new("OrderPlaced"),
    );
    let cancelled = EventType::with_variant(
        Family::new("saga.upstream"),
        TypeName::new("OrderEvent"),
        Variant::new("OrderCancelled"),
    );

    assert_eq!(placed.type_key(), cancelled.type_key());
}

/// Given two EventTypes differing in type_name, When their type-keys are
/// compared, Then the keys differ.
#[test]
fn type_key_differs_when_type_name_differs() {
    let a = EventType::new(Family::new("saga.upstream"), TypeName::new("OrderEvent"));
    let b = EventType::new(Family::new("saga.upstream"), TypeName::new("PaymentEvent"));

    assert_ne!(a.type_key(), b.type_key());
}

/// Given two EventTypes differing in family, When their type-keys are compared,
/// Then the keys differ.
#[test]
fn type_key_differs_when_family_differs() {
    let a = EventType::new(Family::new("saga.upstream"), TypeName::new("OrderEvent"));
    let b = EventType::new(Family::new("saga.downstream"), TypeName::new("OrderEvent"));

    assert_ne!(a.type_key(), b.type_key());
}

/// Given a decode registry keyed by type-key, When a value-level variant is
/// looked up, Then it resolves to the type-registered entry regardless of
/// variant.
#[test]
fn type_key_usable_as_registry_lookup_key() {
    let registered = EventType::new(Family::new("saga.upstream"), TypeName::new("OrderEvent"));
    let mut registry: HashMap<_, &str> = HashMap::new();
    registry.insert(registered.type_key(), "OrderDecoder");

    let loaded_variant = EventType::with_variant(
        Family::new("saga.upstream"),
        TypeName::new("OrderEvent"),
        Variant::new("OrderPlaced"),
    );

    assert_eq!(
        registry.get(&loaded_variant.type_key()),
        Some(&"OrderDecoder")
    );
}

// ---------------------------------------------------------------------------
// Eq / Hash — all three components participate
// ---------------------------------------------------------------------------

/// Given two EventTypes with identical components, When compared, Then they are
/// equal and hash equally.
#[test]
fn equality_holds_when_all_three_components_match() {
    let a = EventType::with_variant(
        Family::new("saga.upstream"),
        TypeName::new("OrderEvent"),
        Variant::new("OrderPlaced"),
    );
    let b = EventType::with_variant(
        Family::new("saga.upstream"),
        TypeName::new("OrderEvent"),
        Variant::new("OrderPlaced"),
    );

    assert_eq!(a, b);
    assert_eq!(hash_of(&a), hash_of(&b));
}

/// Given two EventTypes identical except one has variant None and the other
/// Some, When compared, Then they are NOT equal (variant is part of identity).
#[test]
fn inequality_holds_when_variant_presence_differs() {
    let without = EventType::new(Family::new("saga.upstream"), TypeName::new("OrderEvent"));
    let with = EventType::with_variant(
        Family::new("saga.upstream"),
        TypeName::new("OrderEvent"),
        Variant::new("OrderPlaced"),
    );

    assert_ne!(without, with);
}

/// Given two EventTypes differing only in their Some variant, When compared,
/// Then they are NOT equal.
#[test]
fn inequality_holds_when_some_variants_differ() {
    let placed = EventType::with_variant(
        Family::new("saga.upstream"),
        TypeName::new("OrderEvent"),
        Variant::new("OrderPlaced"),
    );
    let cancelled = EventType::with_variant(
        Family::new("saga.upstream"),
        TypeName::new("OrderEvent"),
        Variant::new("OrderCancelled"),
    );

    assert_ne!(placed, cancelled);
}

/// Given variant-distinguished EventTypes, When inserted into a HashSet, Then
/// each is a distinct member (Hash mirrors Eq over all three components).
#[test]
fn hash_set_distinguishes_variants() {
    let mut set = HashSet::new();
    set.insert(EventType::new(
        Family::new("saga.upstream"),
        TypeName::new("OrderEvent"),
    ));
    set.insert(EventType::with_variant(
        Family::new("saga.upstream"),
        TypeName::new("OrderEvent"),
        Variant::new("OrderPlaced"),
    ));
    set.insert(EventType::with_variant(
        Family::new("saga.upstream"),
        TypeName::new("OrderEvent"),
        Variant::new("OrderCancelled"),
    ));

    assert_eq!(set.len(), 3);
}

// ---------------------------------------------------------------------------
// Display — one-way `family.type_name[.variant]`
// ---------------------------------------------------------------------------

/// Given a struct EventType (variant None) with a non-empty family, When
/// formatted, Then it renders `family.type_name`.
#[test]
fn display_renders_family_and_type_name_without_variant() {
    let et = EventType::new(
        Family::new("nitinol.saga.outbox"),
        TypeName::new("tell_requested"),
    );
    assert_eq!(et.to_string(), "nitinol.saga.outbox.tell_requested");
}

/// Given an enum-arm EventType (variant Some) with a non-empty family, When
/// formatted, Then it appends `.variant`.
#[test]
fn display_appends_variant_when_present() {
    let et = EventType::with_variant(
        Family::new("saga.upstream"),
        TypeName::new("OrderEvent"),
        Variant::new("OrderPlaced"),
    );
    assert_eq!(et.to_string(), "saga.upstream.OrderEvent.OrderPlaced");
}

/// Given an EventType with an empty family, When formatted, Then the family
/// prefix (and its dot) is omitted.
#[test]
fn display_omits_prefix_for_empty_family() {
    let et = EventType::new(Family::new(""), TypeName::new("Incremented"));
    assert_eq!(et.to_string(), "Incremented");
}

/// Given an EventType with an empty family and a variant, When formatted, Then
/// only the family prefix is dropped; the variant suffix remains.
#[test]
fn display_omits_prefix_for_empty_family_but_keeps_variant() {
    let et = EventType::with_variant(
        Family::new(""),
        TypeName::new("Order"),
        Variant::new("Placed"),
    );
    assert_eq!(et.to_string(), "Order.Placed");
}

// ---------------------------------------------------------------------------
// const usability & Copy — required for `const EVENT_TYPE` definitions
// ---------------------------------------------------------------------------

const CONST_STRUCT_EVENT: EventType = EventType::new(
    Family::new("nitinol.saga.outbox"),
    TypeName::new("scheduled"),
);
const CONST_VARIANT_EVENT: EventType = EventType::with_variant(
    Family::new("saga.upstream"),
    TypeName::new("OrderEvent"),
    Variant::new("OrderPlaced"),
);

/// Given EventTypes built in const context, When used at runtime, Then their
/// const constructors produced the expected components (const fn contract).
#[test]
fn const_constructors_are_usable_in_const_context() {
    assert_eq!(CONST_STRUCT_EVENT.type_name(), TypeName::new("scheduled"));
    assert_eq!(CONST_STRUCT_EVENT.variant(), None);
    assert_eq!(
        CONST_VARIANT_EVENT.variant(),
        Some(Variant::new("OrderPlaced"))
    );
}

/// Given an EventType is Copy, When passed by value twice, Then the original
/// remains usable (Copy semantics, not move).
#[test]
fn event_type_is_copy() {
    let original = EventType::new(Family::new("saga.upstream"), TypeName::new("OrderEvent"));
    let copied = original;
    // Both bindings remain independently usable because EventType is Copy.
    assert_eq!(original, copied);
    assert_eq!(original.type_name(), TypeName::new("OrderEvent"));
}

// ---------------------------------------------------------------------------
// Round-trip via MaterializedPath
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Component constructor invariants — invalid inputs must be rejected
// ---------------------------------------------------------------------------

/// Given a TypeName containing a dot, When constructed, Then it panics because
/// TypeName must be a single path segment (no sub-segments).
#[test]
#[should_panic]
fn type_name_with_dot_panics() {
    let _ = TypeName::new("Order.Event");
}

/// Given an empty TypeName, When constructed, Then it panics because an empty
/// segment would break the round-trip contract.
#[test]
#[should_panic]
fn type_name_empty_panics() {
    let _ = TypeName::new("");
}

/// Given a Variant containing a dot, When constructed, Then it panics because
/// Variant must be a single path segment (no sub-segments).
#[test]
#[should_panic]
fn variant_with_dot_panics() {
    let _ = Variant::new("A.B");
}

/// Given an empty Variant, When constructed, Then it panics because an empty
/// segment would break the round-trip contract.
#[test]
#[should_panic]
fn variant_empty_panics() {
    let _ = Variant::new("");
}

/// Given a Family with consecutive dots (empty segment), When constructed, Then
/// it panics because every segment must be non-empty.
#[test]
#[should_panic]
fn family_with_consecutive_dots_panics() {
    let _ = Family::new("a..b");
}

/// Given a Family with a leading dot, When constructed, Then it panics because
/// the first segment would be empty.
#[test]
#[should_panic]
fn family_with_leading_dot_panics() {
    let _ = Family::new(".a");
}

/// Given a Family with a trailing dot, When constructed, Then it panics because
/// the last segment would be empty.
#[test]
#[should_panic]
fn family_with_trailing_dot_panics() {
    let _ = Family::new("a.");
}

// ---------------------------------------------------------------------------
// Round-trip via MaterializedPath
// ---------------------------------------------------------------------------

/// Pinned contract: EventType round-trips as a Materialized Path (#66 acceptance
/// criterion).
///
/// `EventType` uses `&'static str` components for `Copy`/`const` semantics and
/// cannot implement `FromStr` directly.  The parse boundary is
/// [`MaterializedPath`]:
///
/// - **Serialise**: `event_type.to_path().to_string()` → canonical wire string.
/// - **Parse**: `wire.parse::<MaterializedPath>()` → owned path.
/// - **Round-trip**: reparsed path equals the projected path (Eq).
///
/// Covers both struct events (variant None) and enum-arm events (variant Some).
#[test]
fn event_type_round_trips_as_materialized_path() {
    let cases: &[EventType] = &[
        EventType::new(
            Family::new("nitinol.saga.outbox"),
            TypeName::new("tell_requested"),
        ),
        EventType::with_variant(
            Family::new("saga.upstream"),
            TypeName::new("OrderEvent"),
            Variant::new("OrderPlaced"),
        ),
        EventType::new(Family::new(""), TypeName::new("Bare")),
    ];
    for &et in cases {
        let path = et.to_path();
        let wire = path.to_string();
        // wire string must equal Display rendering
        assert_eq!(wire, et.to_string(), "wire != Display for {:?}", et);
        // re-parsing the wire string recovers the same MaterializedPath
        let reparsed = wire
            .parse::<MaterializedPath>()
            .unwrap_or_else(|_| panic!("must re-parse `{wire}`"));
        assert_eq!(reparsed, path, "round-trip failed for {:?}", et);
    }
}

// ---------------------------------------------------------------------------
// ParsedEventType — component-level round-trip (#66 acceptance criterion)
// ---------------------------------------------------------------------------

/// Given a struct EventType with a non-empty family, When its canonical wire
/// string is parsed back via `ParsedEventType::from_struct_path`, Then the
/// recovered Family, TypeName, and Variant components match the original.
#[test]
fn parsed_event_type_round_trips_struct_event_with_family() {
    let et = EventType::new(
        Family::new("nitinol.saga.outbox"),
        TypeName::new("tell_requested"),
    );
    let expected = ParsedEventType::from(et);

    let wire = et.to_path().to_string();
    let path: MaterializedPath = wire.parse().unwrap();
    let parsed = ParsedEventType::from_struct_path(path);

    assert_eq!(parsed, expected);
    assert_eq!(parsed.family, "nitinol.saga.outbox");
    assert_eq!(parsed.type_name, "tell_requested");
    assert_eq!(parsed.variant, None);
}

/// Given an enum-arm EventType with a non-empty family, When its canonical wire
/// string is parsed back via `ParsedEventType::try_from_enum_arm_path`, Then
/// the recovered Family, TypeName, and Variant components match the original.
#[test]
fn parsed_event_type_round_trips_enum_arm_event_with_family() {
    let et = EventType::with_variant(
        Family::new("saga.upstream"),
        TypeName::new("OrderEvent"),
        Variant::new("OrderPlaced"),
    );
    let expected = ParsedEventType::from(et);

    let wire = et.to_path().to_string();
    let path: MaterializedPath = wire.parse().unwrap();
    let parsed = ParsedEventType::try_from_enum_arm_path(path).unwrap();

    assert_eq!(parsed, expected);
    assert_eq!(parsed.family, "saga.upstream");
    assert_eq!(parsed.type_name, "OrderEvent");
    assert_eq!(parsed.variant, Some("OrderPlaced".to_owned()));
}

/// Given a struct EventType with an empty family (top-level event), When its
/// canonical wire string is parsed back, Then the recovered family is empty
/// and type_name matches the original.
#[test]
fn parsed_event_type_round_trips_struct_event_with_empty_family() {
    let et = EventType::new(Family::new(""), TypeName::new("Incremented"));
    let expected = ParsedEventType::from(et);

    let wire = et.to_path().to_string();
    let path: MaterializedPath = wire.parse().unwrap();
    let parsed = ParsedEventType::from_struct_path(path);

    assert_eq!(parsed, expected);
    assert_eq!(parsed.family, "");
    assert_eq!(parsed.type_name, "Incremented");
    assert_eq!(parsed.variant, None);
}

/// Given an enum-arm EventType with an empty family (top-level event with
/// variant), When its canonical wire string is parsed back, Then the recovered
/// family is empty and type_name / variant match the original.
#[test]
fn parsed_event_type_round_trips_enum_arm_event_with_empty_family() {
    let et = EventType::with_variant(
        Family::new(""),
        TypeName::new("Order"),
        Variant::new("Placed"),
    );
    let expected = ParsedEventType::from(et);

    let wire = et.to_path().to_string();
    let path: MaterializedPath = wire.parse().unwrap();
    let parsed = ParsedEventType::try_from_enum_arm_path(path).unwrap();

    assert_eq!(parsed, expected);
    assert_eq!(parsed.family, "");
    assert_eq!(parsed.type_name, "Order");
    assert_eq!(parsed.variant, Some("Placed".to_owned()));
}

/// Given a struct event's canonical wire path, When `try_from_enum_arm_path`
/// is called with only a single segment, Then it returns `None` (cannot
/// hold both type_name and variant).
#[test]
fn try_from_enum_arm_path_returns_none_for_single_segment() {
    let et = EventType::new(Family::new(""), TypeName::new("BareEvent"));
    let path = et.to_path();
    // Single-segment path cannot carry both type_name and variant.
    assert!(ParsedEventType::try_from_enum_arm_path(path).is_none());
}

/// Given a ParsedEventType, When formatted, Then its Display matches the
/// original EventType's Display (canonical string form is identical).
#[test]
fn parsed_event_type_display_matches_event_type_display() {
    let cases: &[EventType] = &[
        EventType::new(
            Family::new("nitinol.saga.outbox"),
            TypeName::new("scheduled"),
        ),
        EventType::with_variant(
            Family::new("saga.upstream"),
            TypeName::new("OrderEvent"),
            Variant::new("OrderPlaced"),
        ),
        EventType::new(Family::new(""), TypeName::new("Bare")),
        EventType::with_variant(
            Family::new(""),
            TypeName::new("Order"),
            Variant::new("Placed"),
        ),
    ];
    for &et in cases {
        let parsed = ParsedEventType::from(et);
        assert_eq!(
            parsed.to_string(),
            et.to_string(),
            "Display mismatch for {:?}",
            et
        );
    }
}
