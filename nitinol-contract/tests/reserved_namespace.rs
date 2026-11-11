//! The `nitinol` reserved namespace — one law, two enforcement points, one
//! definition.
//!
//! The law is part of the identity a domain promises the machinery: `nitinol`
//! and everything beneath it belong to the framework, so a domain names neither
//! an event family nor a stream key there. The root and the boundary rule are
//! defined once, here beside [`Family`], and both enforcement points read them
//! from here. This file fixes the family side, which is enforced by the
//! constructors this crate owns while the constant that holds a family is
//! evaluated. The stream-key side is enforced where identifiers are built, and
//! is fixed by `nitinol-persistence`'s own `reserved_namespace` tests.
//!
//! The boundary is the same one [`MaterializedPath::is_within`] uses for event
//! types: `nitinol` is reserved as a *path root*, so `nitinol.saga` is inside it
//! while `nitinolx` is a different name that merely starts with the same
//! letters. A `starts_with("nitinol")` check would confiscate user names the law
//! never claimed.
//!
//! [`MaterializedPath::is_within`]: nitinol_contract::MaterializedPath::is_within

use nitinol_contract::__private::{reserved_event_registry_entry, reserved_family};
use nitinol_contract::{
    is_within_reserved_namespace, EventRegistryEntry, EventType, Family, TypeName,
    RESERVED_NAMESPACE,
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

/// An entry carries a `&'static [EventType]`, so the event types it would claim
/// are named here rather than borrowed from a call.
static RESERVED_CLAIM: &[EventType] = &[FRAMEWORK_MARKER];

static FRAMEWORK_ENTRY: EventRegistryEntry =
    reserved_event_registry_entry("reserved_namespace::OutboxEvent", RESERVED_CLAIM);

// An entry claiming a reserved family is refused the same way, because a
// declaration reaches the column through its entry as much as through its
// `EVENT_TYPE`.

const OPENED: EventType = EventType::new(Family::new("shop.order"), TypeName::new("Opened"));
static OPENED_CLAIM: &[EventType] = &[OPENED];

#[test]
#[should_panic(expected = "reserved namespace")]
fn event_registry_entry_new_refuses_an_event_type_of_a_reserved_family() {
    let _ = EventRegistryEntry::new("reserved_namespace::Impostor", RESERVED_CLAIM);
}

/// Given an entry claiming an event type outside the namespace, When it is
/// built, Then it is accepted and tells the machinery exactly what it was
/// declared with: the declaring type, and the event types it claims.
///
/// Those two are what the machinery collects to restore a row's declaration
/// and to name both sides of a collision, so an entry that dropped or swapped
/// either would leave the declaration unreachable.
#[test]
fn event_registry_entry_new_accepts_an_event_type_outside_the_namespace() {
    let entry = EventRegistryEntry::new("reserved_namespace::Opened", OPENED_CLAIM);

    assert_eq!(entry.declared_by(), "reserved_namespace::Opened");
    assert_eq!(entry.event_types(), OPENED_CLAIM);
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

/// Given the framework's entry constructor and an event type of a reserved
/// family, When the entry is built, Then it is admitted and carries what it was
/// declared with — the counterpart of `reserved_family` on the entry side.
#[test]
fn the_framework_entry_constructor_admits_a_reserved_family() {
    assert_eq!(
        FRAMEWORK_ENTRY.declared_by(),
        "reserved_namespace::OutboxEvent"
    );
    assert_eq!(FRAMEWORK_ENTRY.event_types(), RESERVED_CLAIM);
}
