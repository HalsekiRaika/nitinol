//! Contract tests for the owned [`MaterializedPath`] (Issue #66).
//!
//! `EventType` stays a `Copy`, `const`-usable identity built from
//! `&'static str` components. `MaterializedPath` is the owned, parseable
//! projection of that identity: a `.`-delimited segment list that round-trips
//! through its `Display`/`FromStr` (canonical string form) and answers
//! hierarchical, segment-boundary prefix questions via `is_within`.
//!
//! The canonical string of a path must equal the one-way `EventType::Display`
//! rendering (`family.type_name[.variant]`, family prefix omitted when empty),
//! so a single wire column can be produced from either side.

use nitinol_persistence::{EventType, Family, MaterializedPath, TypeName, Variant};

fn path(s: &str) -> MaterializedPath {
    s.parse::<MaterializedPath>()
        .unwrap_or_else(|_| panic!("`{s}` must parse as a MaterializedPath"))
}

// ---------------------------------------------------------------------------
// Projection from EventType — to_path() / From<&EventType>
// ---------------------------------------------------------------------------

/// Given a struct EventType (variant None) with a non-empty family, When
/// projected to a path, Then the canonical string is `family.type_name`.
#[test]
fn to_path_renders_family_and_type_name_for_struct_event() {
    let et = EventType::new(
        Family::new("nitinol.saga.outbox"),
        TypeName::new("tell_requested"),
    );
    assert_eq!(
        et.to_path().to_string(),
        "nitinol.saga.outbox.tell_requested"
    );
}

/// Given an enum-arm EventType (variant Some), When projected to a path, Then
/// the variant is appended as the final segment.
#[test]
fn to_path_appends_variant_segment_for_enum_arm_event() {
    let et = EventType::with_variant(
        Family::new("saga.upstream"),
        TypeName::new("OrderEvent"),
        Variant::new("OrderPlaced"),
    );
    assert_eq!(
        et.to_path().to_string(),
        "saga.upstream.OrderEvent.OrderPlaced"
    );
}

/// Given an EventType with an empty family, When projected to a path, Then the
/// family prefix (and its dot) is omitted, matching `EventType::Display`.
#[test]
fn to_path_omits_prefix_for_empty_family() {
    let et = EventType::new(Family::new(""), TypeName::new("Incremented"));
    assert_eq!(et.to_path().to_string(), "Incremented");
}

/// Given an EventType with an empty family and a variant, When projected, Then
/// only the family prefix is dropped; the variant suffix remains.
#[test]
fn to_path_omits_prefix_for_empty_family_but_keeps_variant() {
    let et = EventType::with_variant(
        Family::new(""),
        TypeName::new("Order"),
        Variant::new("Placed"),
    );
    assert_eq!(et.to_path().to_string(), "Order.Placed");
}

/// Given the canonical string of any EventType, When compared to that
/// EventType's `Display`, Then the two renderings are identical (the path's
/// canonical form is the single-column projection of the identity).
#[test]
fn path_canonical_string_equals_event_type_display() {
    let cases = [
        EventType::new(
            Family::new("nitinol.saga.outbox"),
            TypeName::new("scheduled"),
        ),
        EventType::with_variant(
            Family::new("saga.upstream"),
            TypeName::new("OrderEvent"),
            Variant::new("OrderCancelled"),
        ),
        EventType::new(Family::new(""), TypeName::new("Bare")),
    ];
    for et in cases {
        assert_eq!(et.to_path().to_string(), et.to_string());
    }
}

/// Given `From<&EventType>`, When a path is built from an EventType reference,
/// Then it equals the path produced by `to_path` (the two entry points agree).
#[test]
fn from_event_type_reference_matches_to_path() {
    let et = EventType::with_variant(
        Family::new("saga.upstream"),
        TypeName::new("OrderEvent"),
        Variant::new("OrderPlaced"),
    );
    assert_eq!(MaterializedPath::from(&et), et.to_path());
}

// ---------------------------------------------------------------------------
// String round-trip — Display / FromStr
// ---------------------------------------------------------------------------

/// Given a canonical multi-segment string, When parsed and re-rendered, Then
/// the original string is reproduced exactly (round-trip is loss-free).
#[test]
fn parse_then_display_round_trips_multi_segment_path() {
    let s = "nitinol.saga.outbox.tell_requested";
    assert_eq!(path(s).to_string(), s);
}

/// Given a single-segment string (empty-family struct event), When parsed and
/// re-rendered, Then the original string is reproduced.
#[test]
fn parse_then_display_round_trips_single_segment_path() {
    let s = "Incremented";
    assert_eq!(path(s).to_string(), s);
}

/// Given a path projected from an EventType, When re-parsed from its canonical
/// string, Then the re-parsed path equals the projected one (Eq round-trip).
#[test]
fn projected_path_round_trips_through_parse() {
    let et = EventType::with_variant(
        Family::new("saga.upstream"),
        TypeName::new("OrderEvent"),
        Variant::new("OrderPlaced"),
    );
    let projected = et.to_path();
    let reparsed = projected
        .to_string()
        .parse::<MaterializedPath>()
        .expect("must re-parse");
    assert_eq!(reparsed, projected);
}

// ---------------------------------------------------------------------------
// is_within — hierarchical prefix match on segment boundaries
// ---------------------------------------------------------------------------

/// Given a descendant path, When compared against an ancestor prefix, Then it
/// is reported as within that ancestor (deeper segments are allowed).
#[test]
fn is_within_true_for_descendant_on_segment_boundary() {
    let child = path("nitinol.saga.outbox.tell_requested");
    let ancestor = path("nitinol.saga");
    assert!(child.is_within(&ancestor));
}

/// Given a path equal to the prefix, When compared, Then it is reported as
/// within (a path contains itself — exact match is the degenerate prefix).
#[test]
fn is_within_true_for_exact_match() {
    let p = path("saga.upstream.OrderEvent.OrderPlaced");
    assert!(p.is_within(&p));
}

/// Given a path sharing a raw string prefix but crossing no segment boundary,
/// When compared, Then it is NOT within (`nitinol.sagax` is not under
/// `nitinol.saga`).
#[test]
fn is_within_false_for_raw_prefix_without_segment_boundary() {
    let sibling = path("nitinol.sagax.outbox");
    let ancestor = path("nitinol.saga");
    assert!(!sibling.is_within(&ancestor));
}

/// Given a variant path and a type-level prefix that shares the type name but
/// diverges on the last segment, When compared, Then it is NOT within
/// (`...OrderEvent.OrderCancelled` is not under `...OrderEvent.OrderPlaced`).
#[test]
fn is_within_false_when_final_segment_diverges() {
    let cancelled = path("saga.upstream.OrderEvent.OrderCancelled");
    let placed = path("saga.upstream.OrderEvent.OrderPlaced");
    assert!(!cancelled.is_within(&placed));
}

/// Given an ancestor deeper than the path, When compared, Then the shallower
/// path is NOT within the deeper one (direction matters).
#[test]
fn is_within_false_when_ancestor_is_deeper() {
    let shallow = path("nitinol.saga");
    let deeper = path("nitinol.saga.outbox.tell_requested");
    assert!(!shallow.is_within(&deeper));
}

/// Given two unrelated paths, When compared, Then neither is within the other.
#[test]
fn is_within_false_for_unrelated_paths() {
    let a = path("nitinol.saga.outbox");
    let b = path("nitinol.projection.checkpoint");
    assert!(!a.is_within(&b));
}

/// Given a type-level prefix and its variant descendant, When the descendant is
/// tested against the prefix, Then it is within — a type-name prefix matches all
/// of that type's enum arms.
#[test]
fn is_within_true_for_type_prefix_matching_all_variants() {
    let type_prefix = path("saga.upstream.OrderEvent");
    let placed = path("saga.upstream.OrderEvent.OrderPlaced");
    let cancelled = path("saga.upstream.OrderEvent.OrderCancelled");
    assert!(placed.is_within(&type_prefix));
    assert!(cancelled.is_within(&type_prefix));
}
