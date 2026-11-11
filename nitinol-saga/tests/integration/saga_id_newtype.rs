//! `SagaId` is an independent newtype distinct from `AggregateId`.
//!
//! Previously `pub type SagaId = AggregateId`.  The two are now split apart so
//! each can carry its own domain semantics, while a shared `Borrow<str>` bound
//! lets both serve as keys to the same `EventStore`.

use std::borrow::Borrow;

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
