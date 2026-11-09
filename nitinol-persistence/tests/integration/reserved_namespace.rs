//! The `nitinol` reserved namespace on the stream-key side.
//!
//! The law — `nitinol` and everything beneath it belong to the framework — and
//! its boundary rule are defined once, in `nitinol-contract`, beside `Family`.
//! It has two enforcement points, each placed with the side that knows the
//! name: event families are refused by the constructors in `nitinol-contract`
//! while the constant holding them is evaluated (fixed by that crate's
//! `reserved_namespace` tests), and stream keys are refused here, by
//! `reject_reserved_id`, when an identifier is constructed. This file fixes the
//! second point, and that a framework record admitted by the reserved
//! constructors is restored by the registry this crate builds.
//!
//! The boundary is the same one [`MaterializedPath::is_within`] uses for event
//! types: `nitinol` is reserved as a *path root*, so `nitinol.saga` is inside it
//! while `nitinolx` is a different name that merely starts with the same
//! letters. A `starts_with("nitinol")` check would confiscate user identifiers
//! the law never claimed.
//!
//! [`MaterializedPath::is_within`]: nitinol_persistence::MaterializedPath::is_within

use nitinol_persistence::__private::{
    event_registry_from_entries, reserved_event_registry_entry, reserved_family,
};
use nitinol_persistence::{AggregateId, EventRegistryEntry, EventType, Family, TypeName};

/// Names inside the reserved namespace, and names outside it that a naive
/// prefix test would wrongly claim.
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

// The identifiers this crate owns are the ones below. Every constructor in the
// framework routes its refusal through `reject_reserved_id`, so the ones living
// in other crates are fixed beside their own definitions:
// `nitinol-eventsource/tests/projection_id.rs` for a projection's identifier
// and `nitinol-saga/tests/saga_id_reserved_namespace.rs` for a saga's.

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
    }
}

// The framework's own way in, reached through this crate's `__private` the way
// the framework's records reach it. It is `__private` because it is not a user
// surface: it exists so the records nitinol writes can carry the names nitinol
// reserved, and for nothing else.

const FRAMEWORK_FAMILY: Family = reserved_family("nitinol.saga");
const FRAMEWORK_MARKER: EventType = EventType::new(FRAMEWORK_FAMILY, TypeName::new("outbox"));

static FRAMEWORK_ENTRY: EventRegistryEntry =
    reserved_event_registry_entry("reserved_namespace::OutboxEvent", &[FRAMEWORK_MARKER]);

#[test]
fn an_entry_built_by_the_framework_constructor_resolves_in_a_registry() {
    let registry = event_registry_from_entries([&FRAMEWORK_ENTRY])
        .expect("a framework entry claiming one canonical path builds a registry");

    assert_eq!(
        registry.resolve("nitinol.saga.outbox"),
        Some(FRAMEWORK_MARKER),
        "a framework record is restored from the path it is stored under, the same way a user \
         declaration is — the reserved constructor changes who may declare the name, not what \
         the registry does with it"
    );
}
