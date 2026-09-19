//! The `nitinol` reserved namespace — one law, two spaces, one definition.
//!
//! Stream keys (identifiers, checked when they are constructed) and event
//! families (compile-time literals, checked while the constant that holds them
//! is evaluated) are the two spaces the law spans. The root and the boundary
//! rule are defined once, beside [`Family`], and both enforcement points read
//! them from there.
//!
//! The boundary is the same one [`MaterializedPath::is_within`] uses for event
//! types: `nitinol` is reserved as a *path root*, so `nitinol.saga` is inside it
//! while `nitinolx` is a different name that merely starts with the same
//! letters. A `starts_with("nitinol")` check would confiscate user identifiers
//! the law never claimed.
//!
//! [`MaterializedPath::is_within`]: nitinol_persistence::MaterializedPath::is_within

use nitinol_persistence::__private::{reserved_event_registry_entry, reserved_family};
use nitinol_persistence::{
    is_within_reserved_namespace, AggregateId, EventRegistry, EventRegistryEntry, EventType,
    Family, ProjectionId, TypeName, RESERVED_NAMESPACE,
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
         must avoid and what a rejection names"
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

// The predicate is evaluated while a `const` is being evaluated, because that is
// where a family literal is refused: `Family::new` runs inside the initializer
// of a `static`/`const`, and a refusal there is a compile error rather than a
// condition a running deployment discovers. A predicate that could only run at
// run time could not be read from there at all — these two constants stop
// compiling if it stops being const-evaluable.

const ROOT_IS_INSIDE: bool = is_within_reserved_namespace(RESERVED_NAMESPACE);
const A_NAME_BENEATH_IS_INSIDE: bool = is_within_reserved_namespace("nitinol.saga");
const A_LOOKALIKE_IS_OUTSIDE: bool = is_within_reserved_namespace("nitinolx");
const THE_EMPTY_FAMILY_IS_OUTSIDE: bool = is_within_reserved_namespace("");

#[test]
fn the_boundary_answers_the_same_while_a_const_is_being_evaluated() {
    for (const_answer, candidate) in [
        (ROOT_IS_INSIDE, RESERVED_NAMESPACE),
        (A_NAME_BENEATH_IS_INSIDE, "nitinol.saga"),
        (A_LOOKALIKE_IS_OUTSIDE, "nitinolx"),
        (THE_EMPTY_FAMILY_IS_OUTSIDE, ""),
    ] {
        assert_eq!(
            const_answer,
            is_within_reserved_namespace(candidate),
            "{candidate:?} must be classified the same while a const is evaluated as it is at \
             run time"
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

// Families inside the namespace are refused by the constructor user code reaches

#[test]
#[should_panic(expected = "reserved namespace")]
fn family_new_refuses_the_reserved_namespace_root() {
    let _ = Family::new("nitinol");
}

#[test]
#[should_panic(expected = "reserved namespace")]
fn family_new_refuses_a_family_inside_the_reserved_namespace() {
    let _ = Family::new("nitinol.saga");
}

#[test]
#[should_panic(expected = "reserved namespace")]
fn family_new_refuses_a_family_beneath_a_framework_one() {
    let _ = Family::new("nitinol.saga.outbox");
}

/// The refusal must not spill over onto families that merely share a prefix
/// with the reserved root, nor onto ordinary ones — including the empty family,
/// which is an explicit supported value.
#[test]
fn family_new_accepts_every_family_outside_the_reserved_namespace() {
    for (candidate, reserved) in CLASSIFICATION {
        if *reserved {
            continue;
        }
        assert_eq!(
            Family::new(candidate).as_str(),
            *candidate,
            "Family::new must accept {candidate:?} unchanged"
        );
    }
}

// The framework's own way in. It is `__private` because it is not a user
// surface: it exists so the records nitinol writes can carry the names nitinol
// reserved, and for nothing else.

const FRAMEWORK_FAMILY: Family = reserved_family("nitinol.saga");
const FRAMEWORK_MARKER: EventType = EventType::new(FRAMEWORK_FAMILY, TypeName::new("outbox"));

static FRAMEWORK_ENTRY: EventRegistryEntry =
    reserved_event_registry_entry("reserved_namespace::OutboxEvent", &[FRAMEWORK_MARKER]);

// An entry claiming a reserved family is refused the same way, because a
// declaration reaches the column through its entry as much as through its
// `EVENT_TYPE`.

/// An entry carries a `&'static [EventType]`, so the event types it would claim
/// are named here rather than borrowed from a call.
static RESERVED_CLAIM: &[EventType] = &[FRAMEWORK_MARKER];

const OPENED: EventType = EventType::new(Family::new("shop.order"), TypeName::new("Opened"));
static OPENED_CLAIM: &[EventType] = &[OPENED];

#[test]
#[should_panic(expected = "reserved namespace")]
fn event_registry_entry_new_refuses_an_event_type_of_a_reserved_family() {
    let _ = EventRegistryEntry::new("reserved_namespace::Impostor", RESERVED_CLAIM);
}

#[test]
fn event_registry_entry_new_accepts_an_event_type_outside_the_namespace() {
    let entry = EventRegistryEntry::new("reserved_namespace::Opened", OPENED_CLAIM);

    let registry = EventRegistry::from_entries([&entry])
        .expect("an entry outside the reserved namespace builds a registry");
    assert_eq!(registry.resolve("shop.order.Opened"), Some(OPENED));
}

#[test]
fn the_framework_constructor_admits_a_reserved_family() {
    assert_eq!(
        FRAMEWORK_FAMILY.as_str(),
        "nitinol.saga",
        "the reserved constructor carries the name through unchanged — it is the one way a \
         framework record gets the family the user constructor refuses"
    );
    assert!(
        is_within_reserved_namespace(FRAMEWORK_FAMILY.as_str()),
        "the family it admitted is inside the namespace, which is the whole reason it exists"
    );
    assert_eq!(FRAMEWORK_MARKER.to_string(), "nitinol.saga.outbox");
}

#[test]
fn the_framework_constructor_still_enforces_the_shape_of_a_family() {
    // The reserved constructor waives the namespace check and nothing else: a
    // malformed family is malformed whoever declares it.
    let refused = std::panic::catch_unwind(|| reserved_family("nitinol..saga"));
    assert!(
        refused.is_err(),
        "a family with an empty segment is refused on every constructor, framework-owned included"
    );
}

#[test]
fn an_entry_built_by_the_framework_constructor_resolves_in_a_registry() {
    let registry = EventRegistry::from_entries([&FRAMEWORK_ENTRY])
        .expect("a framework entry claiming one canonical path builds a registry");

    assert_eq!(
        registry.resolve("nitinol.saga.outbox"),
        Some(FRAMEWORK_MARKER),
        "a framework record is restored from the path it is stored under, the same way a user \
         declaration is — the reserved constructor changes who may declare the name, not what \
         the registry does with it"
    );
}
