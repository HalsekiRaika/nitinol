//! Contract tests for [`EventType`] — the one grammar a journal's `event_type`
//! column is written in.
//!
//! An `EventType` is a `Family` and a `TypeName`, and nothing else. A `TypeName`
//! is a single segment by construction, so the canonical path decomposes
//! structurally and without ambiguity: **the last segment is the `TypeName`, and
//! every segment before it is the `family`**. That is what makes the column's
//! grammar unique — one path, one reading, one type.
//!
//! # Round-trip via `MaterializedPath`
//!
//! `EventType` uses `&'static str` for both components and is therefore `Copy`
//! and `const`-constructable, but cannot implement `FromStr` (runtime strings
//! cannot be borrowed as `&'static str`). The parse boundary is
//! [`MaterializedPath`]: `event_type.to_path().to_string()` produces the
//! canonical wire string, which `str::parse::<MaterializedPath>()` recovers
//! without loss, and [`ParsedEventType::from_path`] decomposes back into owned
//! components.
//!
//! Families used here stay outside the `nitinol` reserved namespace: the
//! namespace is framework-owned and [`Family::new`] refuses it, so a test about
//! segment boundaries has to pick a name a user could actually declare.
//! `reserved_namespace.rs` owns the refusal itself.

use std::collections::hash_map::DefaultHasher;
use std::collections::HashSet;
use std::hash::{Hash, Hasher};

use nitinol_persistence::{EventType, Family, MaterializedPath, ParsedEventType, TypeName};

fn hash_of<T: Hash>(value: &T) -> u64 {
    let mut hasher = DefaultHasher::new();
    value.hash(&mut hasher);
    hasher.finish()
}

/// Every shape a canonical path has: a deep family, a one-segment family, and
/// no family at all.
fn every_shape() -> [EventType; 3] {
    [
        EventType::new(
            Family::new("shop.order.outbox"),
            TypeName::new("TellRequested"),
        ),
        EventType::new(Family::new("shop"), TypeName::new("Opened")),
        EventType::new(Family::new(""), TypeName::new("Incremented")),
    ]
}

// Component newtypes: Family / TypeName

/// Given a Family created from a str, When as_str is read, Then the original
/// str is returned unchanged.
#[test]
fn family_as_str_round_trips_the_source_string() {
    let family = Family::new("shop.order.outbox");
    assert_eq!(family.as_str(), "shop.order.outbox");
}

/// Given a TypeName created from a str, When as_str is read, Then the original
/// str is returned unchanged.
#[test]
fn type_name_as_str_round_trips_the_source_string() {
    let type_name = TypeName::new("TellRequested");
    assert_eq!(type_name.as_str(), "TellRequested");
}

// Family::is_within — hierarchical prefix match on segment boundaries

/// Given a descendant family, When compared against an ancestor prefix, Then it
/// is reported as within that ancestor.
#[test]
fn family_is_within_true_for_descendant_on_segment_boundary() {
    let child = Family::new("shop.order.outbox");
    let ancestor = Family::new("shop.order");
    assert!(child.is_within(ancestor));
}

/// Given a family equal to the ancestor, When compared, Then it is reported as
/// within (a family contains itself).
#[test]
fn family_is_within_true_for_exact_match() {
    let family = Family::new("shop.order");
    assert!(family.is_within(family));
}

/// Given a family that shares a raw string prefix but crosses no segment
/// boundary, When compared, Then it is reported as NOT within (`shop.orderx`
/// is not under `shop.order`).
#[test]
fn family_is_within_false_for_raw_prefix_without_segment_boundary() {
    let sibling = Family::new("shop.orderx");
    let ancestor = Family::new("shop.order");
    assert!(!sibling.is_within(ancestor));
}

/// Given an ancestor deeper than the family, When compared, Then the shallower
/// family is NOT within the deeper one (direction matters).
#[test]
fn family_is_within_false_when_ancestor_is_deeper() {
    let shallow = Family::new("shop.order");
    let deeper = Family::new("shop.order.outbox");
    assert!(!shallow.is_within(deeper));
}

/// Given two unrelated families, When compared, Then neither is within the
/// other.
#[test]
fn family_is_within_false_for_unrelated_families() {
    let a = Family::new("shop.order");
    let b = Family::new("shop.projection");
    assert!(!a.is_within(b));
}

// EventType construction and accessors

/// Given an EventType, When its components are read, Then family and type_name
/// are exactly the two it was built from — an event type carries no third
/// component for a stored row to disagree with.
#[test]
fn an_event_type_is_its_family_and_its_type_name() {
    let et = EventType::new(
        Family::new("shop.order.outbox"),
        TypeName::new("TellRequested"),
    );

    assert_eq!(et.family(), Family::new("shop.order.outbox"));
    assert_eq!(et.type_name(), TypeName::new("TellRequested"));
}

// The grammar: last segment is the TypeName, everything before it is the family

/// Given an event type of any shape, When it is projected to its canonical
/// path, Then the path holds the family's segments followed by exactly one more
/// — the type name.
///
/// This is the whole of the grammar: a path carries no segment beyond the type
/// name, so a reader splitting on the last segment recovers the one declaration
/// the path was written for.
#[test]
fn a_canonical_path_is_the_family_segments_followed_by_the_type_name_alone() {
    for et in every_shape() {
        let rendered = et.to_path().to_string();
        let segments: Vec<&str> = rendered.split('.').collect();

        let family = et.family().as_str();
        let expected_family_segments = if family.is_empty() {
            Vec::new()
        } else {
            family.split('.').collect::<Vec<_>>()
        };

        assert_eq!(
            segments.len(),
            expected_family_segments.len() + 1,
            "a canonical path holds the family's segments and the type name, and {rendered:?} \
             holds a different number — a further segment would leave the path readable as more \
             than one declaration"
        );
        assert_eq!(
            &segments[..expected_family_segments.len()],
            &expected_family_segments[..],
            "the leading segments of {rendered:?} are the family"
        );
        assert_eq!(
            segments[segments.len() - 1],
            et.type_name().as_str(),
            "the last segment of {rendered:?} is the type name"
        );
    }
}

/// Given two event types whose canonical paths are equal, When they are
/// compared, Then they are the same event type.
///
/// One path answers for one declaration. A family that swallows what another
/// declaration reads as its type name would break this, and it is what the
/// registry's collision check refuses at build time.
#[test]
fn one_canonical_path_decomposes_into_one_pair_of_components() {
    let declared = EventType::new(Family::new("a.b"), TypeName::new("C"));

    let path: MaterializedPath = declared
        .to_path()
        .to_string()
        .parse()
        .expect("a canonical path re-parses");
    let parsed = ParsedEventType::from_path(path);

    assert_eq!(parsed.family, "a.b");
    assert_eq!(parsed.type_name, "C");
    assert_eq!(
        parsed,
        ParsedEventType::from(declared),
        "decomposing the path a declaration is written down as recovers that declaration's own \
         components, with no second reading to choose between"
    );
}

// Eq / Hash — both components participate

/// Given two EventTypes with identical components, When compared, Then they are
/// equal and hash equally.
#[test]
fn equality_holds_when_both_components_match() {
    let a = EventType::new(Family::new("saga.upstream"), TypeName::new("OrderEvent"));
    let b = EventType::new(Family::new("saga.upstream"), TypeName::new("OrderEvent"));

    assert_eq!(a, b);
    assert_eq!(hash_of(&a), hash_of(&b));
}

/// Given two EventTypes differing in type_name, When compared, Then they are
/// not equal.
#[test]
fn inequality_holds_when_type_name_differs() {
    let a = EventType::new(Family::new("saga.upstream"), TypeName::new("OrderEvent"));
    let b = EventType::new(Family::new("saga.upstream"), TypeName::new("PaymentEvent"));

    assert_ne!(a, b);
}

/// Given two EventTypes differing in family, When compared, Then they are not
/// equal.
#[test]
fn inequality_holds_when_family_differs() {
    let a = EventType::new(Family::new("saga.upstream"), TypeName::new("OrderEvent"));
    let b = EventType::new(Family::new("saga.downstream"), TypeName::new("OrderEvent"));

    assert_ne!(a, b);
}

/// Given distinct EventTypes, When inserted into a HashSet, Then each is a
/// distinct member (Hash mirrors Eq over both components).
#[test]
fn hash_set_distinguishes_event_types() {
    let mut set = HashSet::new();
    for et in every_shape() {
        set.insert(et);
    }

    assert_eq!(set.len(), 3);
}

// Display — `family.TypeName`, one way

/// Given an EventType with a non-empty family, When formatted, Then it renders
/// `family.TypeName`.
#[test]
fn display_renders_family_and_type_name() {
    let et = EventType::new(
        Family::new("shop.order.outbox"),
        TypeName::new("TellRequested"),
    );
    assert_eq!(et.to_string(), "shop.order.outbox.TellRequested");
}

/// Given an EventType with an empty family, When formatted, Then the family
/// prefix (and its dot) is omitted.
#[test]
fn display_omits_prefix_for_empty_family() {
    let et = EventType::new(Family::new(""), TypeName::new("Incremented"));
    assert_eq!(et.to_string(), "Incremented");
}

// const usability & Copy — required for `const EVENT_TYPE` definitions

const CONST_EVENT: EventType =
    EventType::new(Family::new("shop.order.outbox"), TypeName::new("Scheduled"));

/// Given an EventType built in const context, When used at runtime, Then its
/// const constructor produced the expected components (const fn contract).
#[test]
fn const_constructors_are_usable_in_const_context() {
    assert_eq!(CONST_EVENT.type_name(), TypeName::new("Scheduled"));
    assert_eq!(CONST_EVENT.family(), Family::new("shop.order.outbox"));
}

// Component constructor invariants — invalid inputs must be rejected

/// Given a TypeName containing a dot, When constructed, Then it panics.
///
/// A dotted type name is what would make the grammar ambiguous: the reader
/// splits on the last segment, so a type name spanning two segments could be
/// read as a deeper family and a shorter name.
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

// Round-trip via MaterializedPath

/// Pinned contract: EventType round-trips as a Materialized Path.
///
/// - **Serialise**: `event_type.to_path().to_string()` → canonical wire string.
/// - **Parse**: `wire.parse::<MaterializedPath>()` → owned path.
/// - **Round-trip**: reparsed path equals the projected path (Eq).
#[test]
fn event_type_round_trips_as_materialized_path() {
    for et in every_shape() {
        let path = et.to_path();
        let wire = path.to_string();
        // wire string must equal Display rendering
        assert_eq!(wire, et.to_string(), "wire != Display for {et:?}");
        // re-parsing the wire string recovers the same MaterializedPath
        let reparsed = wire
            .parse::<MaterializedPath>()
            .unwrap_or_else(|_| panic!("must re-parse `{wire}`"));
        assert_eq!(reparsed, path, "round-trip failed for {et:?}");
    }
}

// ParsedEventType — component-level round-trip

/// Given an EventType of any shape, When its canonical wire string is parsed
/// back via `ParsedEventType::from_path`, Then the recovered components equal
/// the ones the EventType was declared with.
#[test]
fn parsed_event_type_round_trips_every_shape() {
    for et in every_shape() {
        let expected = ParsedEventType::from(et);

        let wire = et.to_path().to_string();
        let path: MaterializedPath = wire.parse().expect("canonical wire string must parse");
        let parsed = ParsedEventType::from_path(path);

        assert_eq!(parsed, expected, "component round-trip failed for {wire:?}");
        assert_eq!(parsed.family, et.family().as_str());
        assert_eq!(parsed.type_name, et.type_name().as_str());
    }
}

/// Given a single-segment path (an event type with no family), When it is
/// decomposed, Then the family is empty and the one segment is the type name.
#[test]
fn a_single_segment_path_decomposes_to_an_empty_family() {
    let path: MaterializedPath = "Bare".parse().expect("a single segment parses");
    let parsed = ParsedEventType::from_path(path);

    assert_eq!(parsed.family, "");
    assert_eq!(parsed.type_name, "Bare");
}

/// Given a ParsedEventType, When formatted, Then its Display matches the
/// original EventType's Display (canonical string form is identical).
#[test]
fn parsed_event_type_display_matches_event_type_display() {
    for et in every_shape() {
        let parsed = ParsedEventType::from(et);
        assert_eq!(parsed.to_string(), et.to_string(), "Display mismatch");
    }
}
