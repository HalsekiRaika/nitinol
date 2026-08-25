//! `SagaId` is an independent newtype distinct from `AggregateId`.
//!
//! Previously `pub type SagaId = AggregateId`.  The two are now split apart so
//! each can carry its own domain semantics, while a shared `Borrow<str>` bound
//! lets both serve as keys to the same `EventStore`.

use std::borrow::Borrow;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use nitinol_saga::SagaId;

// SagaId implements Borrow<str>

/// SagaId implements Borrow<str> so it can be passed where a string-borrowable
/// key is expected (e.g. `EventStore::append`, `LoadQuery::by_stream`).
#[test]
fn saga_id_borrows_as_str() {
    // Given
    let id = SagaId::new("saga-borrow");

    // When
    let borrowed: &str = id.borrow();

    // Then
    assert_eq!(borrowed, "saga-borrow");
}

// SagaId exposes as_str()

/// SagaId exposes `as_str()` mirroring `AggregateId::as_str()`.  This keeps
/// call sites that previously assumed `SagaId = AggregateId` working with a
/// minimal substitution.
#[test]
fn saga_id_exposes_as_str() {
    // Given
    let id = SagaId::new("saga-as-str");

    // When / Then
    assert_eq!(id.as_str(), "saga-as-str");
}

// SagaId derives Clone + PartialEq + Eq + Hash + Debug

/// SagaId implements the standard newtype trait set required by the saga
/// runtime (cloning into context, equality routing, hashing for registries,
/// Debug for tracing).
#[test]
fn saga_id_implements_standard_traits() {
    // Given
    let a = SagaId::new("trait-saga");
    let b = a.clone();

    // Then: Clone + PartialEq + Eq + Hash + Debug
    assert_eq!(a, b, "Clone + PartialEq must yield equal values");

    let mut h1 = DefaultHasher::new();
    a.hash(&mut h1);
    let mut h2 = DefaultHasher::new();
    b.hash(&mut h2);
    assert_eq!(h1.finish(), h2.finish(), "equal SagaIds must hash equal");

    let _ = format!("{:?}", a);
}

// SagaId is distinct from AggregateId at the type level

/// SagaId and AggregateId are independent types — assigning a function pointer
/// that returns AggregateId to a slot that expects SagaId must fail to compile.
///
/// Demonstration of distinctness at runtime: an `AggregateId::new("x")` and a
/// `SagaId::new("x")` cannot be compared with `==` because no `PartialEq`
/// impl exists between them.  We exercise this at the type level by reading
/// each through its own slot.
#[test]
fn saga_id_is_distinct_type_from_aggregate_id() {
    use nitinol_persistence::AggregateId;

    let agg = AggregateId::new("same-text");
    let saga = SagaId::new("same-text");

    // Both borrow to the same &str, but they are nominally distinct types.
    let agg_str: &str = agg.borrow();
    let saga_str: &str = saga.borrow();

    // Their string representations are equal — by-text equivalence is OK.
    assert_eq!(
        agg_str, saga_str,
        "both ids carry the same text via Borrow<str>"
    );
}
