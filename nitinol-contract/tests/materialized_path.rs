//! Contract tests for the owned [`MaterializedPath`].
//!
//! `EventType` stays a `Copy`, `const`-usable identity built from
//! `&'static str` components. `MaterializedPath` is the owned, parseable
//! projection of that identity: a `.`-delimited segment list that round-trips
//! through its `Display`/`FromStr` (canonical string form) and answers
//! hierarchical, segment-boundary prefix questions via `is_within`.
//!
//! The canonical string of a path must equal the one-way `EventType::Display`
//! rendering (`family.TypeName`, family prefix omitted when empty), so a single
//! wire column can be produced from either side.
//!
//! `is_within` is the descendant rule every `event_type_prefix` search rests on,
//! and it is unchanged: a prefix selects itself and everything beneath it, on
//! segment boundaries. What changed is the projection — a path holds the
//! family's segments and the type name, and nothing after it.

use std::error::Error;

use nitinol_contract::{EventType, Family, MaterializedPath, MaterializedPathParseError, TypeName};

fn path(s: &str) -> MaterializedPath {
    s.parse::<MaterializedPath>()
        .unwrap_or_else(|_| panic!("`{s}` must parse as a MaterializedPath"))
}

// Projection from EventType — to_path() / From<&EventType>

/// Given an EventType with a non-empty family, When projected to a path, Then
/// the canonical string is `family.TypeName` — the projection appends the type
/// name and stops.
#[test]
fn to_path_renders_family_then_type_name_and_nothing_further() {
    let et = EventType::new(
        Family::new("shop.order.outbox"),
        TypeName::new("TellRequested"),
    );
    assert_eq!(et.to_path().to_string(), "shop.order.outbox.TellRequested");
}

/// Given an EventType with an empty family, When projected to a path, Then the
/// family prefix (and its dot) is omitted, matching `EventType::Display`.
#[test]
fn to_path_omits_prefix_for_empty_family() {
    let et = EventType::new(Family::new(""), TypeName::new("Incremented"));
    assert_eq!(et.to_path().to_string(), "Incremented");
}

/// Given the canonical string of any EventType, When compared to that
/// EventType's `Display`, Then the two renderings are identical (the path's
/// canonical form is the single-column projection of the identity).
#[test]
fn path_canonical_string_equals_event_type_display() {
    let cases = [
        EventType::new(Family::new("shop.order.outbox"), TypeName::new("Scheduled")),
        EventType::new(Family::new("saga.upstream"), TypeName::new("OrderEvent")),
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
    let et = EventType::new(Family::new("saga.upstream"), TypeName::new("OrderEvent"));
    assert_eq!(MaterializedPath::from(&et), et.to_path());
}

// String round-trip — Display / FromStr

/// Given a canonical multi-segment string, When parsed and re-rendered, Then
/// the original string is reproduced exactly (round-trip is loss-free).
#[test]
fn parse_then_display_round_trips_multi_segment_path() {
    let s = "shop.order.outbox.TellRequested";
    assert_eq!(path(s).to_string(), s);
}

/// Given a single-segment string (empty-family event), When parsed and
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
    let et = EventType::new(Family::new("saga.upstream"), TypeName::new("OrderEvent"));
    let projected = et.to_path();
    let reparsed = projected
        .to_string()
        .parse::<MaterializedPath>()
        .expect("must re-parse");
    assert_eq!(reparsed, projected);
}

// is_within — hierarchical prefix match on segment boundaries

/// Given a descendant path, When compared against an ancestor prefix, Then it
/// is reported as within that ancestor (deeper segments are allowed).
#[test]
fn is_within_true_for_descendant_on_segment_boundary() {
    let child = path("shop.order.outbox.TellRequested");
    let ancestor = path("shop.order");
    assert!(child.is_within(&ancestor));
}

/// Given a path equal to the prefix, When compared, Then it is reported as
/// within (a path contains itself — exact match is the degenerate prefix).
#[test]
fn is_within_true_for_exact_match() {
    let p = path("saga.upstream.OrderEvent");
    assert!(p.is_within(&p));
}

/// Given a path sharing a raw string prefix but crossing no segment boundary,
/// When compared, Then it is NOT within (`shop.orderx` is not under
/// `shop.order`).
#[test]
fn is_within_false_for_raw_prefix_without_segment_boundary() {
    let sibling = path("shop.orderx.outbox");
    let ancestor = path("shop.order");
    assert!(!sibling.is_within(&ancestor));
}

/// Given two paths that diverge on their final segment, When compared, Then
/// neither is within the other — sibling type names under one family stay
/// distinct.
#[test]
fn is_within_false_when_final_segment_diverges() {
    let cancelled = path("saga.upstream.OrderCancelled");
    let placed = path("saga.upstream.OrderPlaced");
    assert!(!cancelled.is_within(&placed));
}

/// Given an ancestor deeper than the path, When compared, Then the shallower
/// path is NOT within the deeper one (direction matters).
#[test]
fn is_within_false_when_ancestor_is_deeper() {
    let shallow = path("shop.order");
    let deeper = path("shop.order.outbox.TellRequested");
    assert!(!shallow.is_within(&deeper));
}

/// Given two unrelated paths, When compared, Then neither is within the other.
#[test]
fn is_within_false_for_unrelated_paths() {
    let a = path("shop.order.outbox");
    let b = path("shop.projection.checkpoint");
    assert!(!a.is_within(&b));
}

/// Given a family-level prefix, When every event type declared beneath it is
/// tested, Then each is within it — which is what a family-prefix search
/// selects, and the reason `is_within` is not narrowed to exact equality.
#[test]
fn is_within_true_for_a_family_prefix_matching_every_type_declared_under_it() {
    let family_prefix = path("saga.upstream");
    for declared in [
        EventType::new(Family::new("saga.upstream"), TypeName::new("OrderPlaced")),
        EventType::new(
            Family::new("saga.upstream"),
            TypeName::new("OrderCancelled"),
        ),
        EventType::new(
            Family::new("saga.upstream.detail"),
            TypeName::new("LineAdded"),
        ),
    ] {
        assert!(
            declared.to_path().is_within(&family_prefix),
            "{declared} is declared under `saga.upstream` and must be selected by that prefix"
        );
    }
}

// Parse errors — what a column value that is not a path is refused with

/// Given an empty string, When parsed, Then it is refused as `Empty` and the
/// message says a path needs at least one segment.
///
/// The wording is what a failed load surfaces to whoever reads the store's
/// error, so it is held to the exact text rather than to "some message".
#[test]
fn an_empty_string_is_refused_as_a_path_without_segments() {
    let refused = ""
        .parse::<MaterializedPath>()
        .expect_err("an empty string has no segment to read a type name from");

    assert!(
        matches!(refused, MaterializedPathParseError::Empty),
        "an empty string is the `Empty` refusal, not an empty segment: {refused:?}"
    );
    assert_eq!(
        refused.to_string(),
        "a materialized path must contain at least one segment"
    );
}

/// Given strings with an empty segment at the front, in the middle and at the
/// end, When parsed, Then each is refused as `EmptySegment` carrying the whole
/// input and the position of the first empty segment, and the message names
/// both.
#[test]
fn an_empty_segment_is_refused_with_the_input_and_its_position() {
    for (input, position) in [(".a", 0), ("a..b", 1), ("a.", 1), ("a.b..c", 2)] {
        let refused = input
            .parse::<MaterializedPath>()
            .expect_err("a path with an empty segment is not a canonical path");

        match &refused {
            MaterializedPathParseError::EmptySegment {
                input: carried,
                index,
            } => {
                assert_eq!(carried, input, "the refusal carries the input it refused");
                assert_eq!(*index, position, "{input:?} is empty at segment {position}");
            }
            MaterializedPathParseError::Empty => {
                panic!("{input:?} has segments, so it is not the `Empty` refusal")
            }
        }
        assert_eq!(
            refused.to_string(),
            format!("materialized path `{input}` has an empty segment at position {position}")
        );
    }
}

/// Given a parse refusal, When it is handled as a `dyn Error`, Then it is one,
/// its message is the same `Display` text, and it has no further source — the
/// refusal is about the string itself, with nothing underneath it to chain.
#[test]
fn a_parse_refusal_is_a_std_error_without_a_source() {
    let refused = "a..b"
        .parse::<MaterializedPath>()
        .expect_err("a path with an empty segment is refused");
    // `Send + Sync + 'static` is what lets a store carry the refusal inside
    // its own error type across an `.await`.
    let as_error: &(dyn Error + Send + Sync + 'static) = &refused;

    assert_eq!(as_error.to_string(), refused.to_string());
    assert!(as_error.source().is_none());
}
