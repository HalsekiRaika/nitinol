//! `SagaId` obeys the same reserved-namespace law as every other identifier.
//!
//! A saga's id *is* its EventStore stream key, and the framework's own record
//! types already live under `nitinol.saga`.  The law and its boundary rule are
//! owned by `nitinol-contract` and pinned there
//! (`nitinol-contract/tests/reserved_namespace.rs`); its stream-key enforcement
//! point is `nitinol-persistence`'s `reject_reserved_id`
//! (`nitinol-persistence/tests/integration/reserved_namespace.rs`); what this file pins is
//! that `SagaId` is subject to it, since a saga id is the one stream key a
//! user names directly.

use nitinol_saga::SagaId;

#[test]
#[should_panic(expected = "reserved namespace")]
fn saga_id_refuses_the_reserved_namespace_root() {
    let _ = SagaId::new("nitinol");
}

#[test]
#[should_panic(expected = "reserved namespace")]
fn saga_id_refuses_a_name_inside_the_reserved_namespace() {
    let _ = SagaId::new("nitinol.saga.orders");
}

/// The refusal is bounded by path segments, so an id that merely starts with
/// the same letters stays a perfectly ordinary user id.
#[test]
fn saga_id_accepts_names_outside_the_reserved_namespace() {
    for candidate in [
        "nitinolx",
        "nitinol-1",
        "nitinol_saga",
        "my.nitinol",
        "order-7",
    ] {
        assert_eq!(
            SagaId::new(candidate).as_str(),
            candidate,
            "SagaId must accept {candidate:?} unchanged"
        );
    }
}
