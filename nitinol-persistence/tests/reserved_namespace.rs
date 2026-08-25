//! The `nitinol` reserved namespace — stream-key side.
//!
//! One law spans two spaces: stream keys (identifiers, checked at run time)
//! and event types (families, checked at compile time by the derive macro).
//! This file pins the identifier half, and the boundary rule both halves share.
//!
//! The boundary is the same one [`MaterializedPath::is_within`] already uses
//! for event types: `nitinol` is reserved as a *path root*, so
//! `nitinol.saga` is inside it while `nitinolx` is a different name that
//! merely starts with the same letters.  A `starts_with("nitinol")` check
//! would confiscate user identifiers the law never claimed.
//!
//! [`MaterializedPath::is_within`]: nitinol_persistence::MaterializedPath::is_within

use nitinol_persistence::{
    is_within_reserved_namespace, AggregateId, ProjectionId, RESERVED_NAMESPACE,
};

/// Names inside the reserved namespace, and names outside it that a naive
/// prefix test would wrongly claim.
///
/// `""` is listed as outside deliberately: an empty family is an explicit,
/// supported value on the event-type side, so the shared predicate must not
/// reject it.
const CLASSIFICATION: &[(&str, bool)] = &[
    ("nitinol", true),
    ("nitinol.saga", true),
    ("nitinol.saga.dead_letter", true),
    ("nitinolx", false),
    ("nitinol-1", false),
    ("nitinol_saga", false),
    ("my.nitinol", false),
    ("order-7", false),
    ("", false),
];

#[test]
fn the_reserved_namespace_is_the_nitinol_path_root() {
    assert_eq!(
        RESERVED_NAMESPACE, "nitinol",
        "the reserved namespace's name is public contract: it is what user code \
         must avoid and what the derive macro's rejection names"
    );
}

#[test]
fn reserved_namespace_membership_is_decided_on_segment_boundaries() {
    for (candidate, reserved) in CLASSIFICATION {
        assert_eq!(
            is_within_reserved_namespace(candidate),
            *reserved,
            "{candidate:?} must{} be inside the reserved namespace",
            if *reserved { "" } else { " not" }
        );
    }
}

// Identifiers inside the namespace are refused

#[test]
#[should_panic(expected = "reserved namespace")]
fn aggregate_id_refuses_the_reserved_namespace_root() {
    let _ = AggregateId::new("nitinol");
}

#[test]
#[should_panic(expected = "reserved namespace")]
fn aggregate_id_refuses_a_name_inside_the_reserved_namespace() {
    let _ = AggregateId::new("nitinol.saga.orders");
}

#[test]
#[should_panic(expected = "reserved namespace")]
fn projection_id_refuses_a_name_inside_the_reserved_namespace() {
    let _ = ProjectionId::new("nitinol.projection");
}

// …and only those

/// The refusal must not spill over onto identifiers that merely share a
/// prefix with the reserved root, nor onto ordinary ones.
#[test]
fn id_constructors_accept_every_name_outside_the_reserved_namespace() {
    for (candidate, reserved) in CLASSIFICATION {
        if *reserved {
            continue;
        }
        assert_eq!(
            AggregateId::new(*candidate).as_str(),
            *candidate,
            "AggregateId must accept {candidate:?} unchanged"
        );
        assert_eq!(
            ProjectionId::new(*candidate).as_str(),
            *candidate,
            "ProjectionId must accept {candidate:?} unchanged"
        );
    }
}
