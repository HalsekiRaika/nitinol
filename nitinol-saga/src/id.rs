//! Saga identifier.
//!
//! A saga instance is uniquely identified by a [`SagaId`].  It is its own
//! newtype — independent of `AggregateId` — so the type system can keep
//! saga-owned streams and aggregate-owned streams distinct, even though both
//! are persisted into the same physical `EventStore` via the shared
//! `Borrow<str>` key abstraction.

use std::borrow::Borrow;

use nitinol_persistence::reject_reserved_id;

/// Identifier for a saga instance.
///
/// Constructed with [`SagaId::new`], borrowed as `&str` via the standard
/// `Borrow<str>` impl for use with `EventStore::append` / `LoadQuery::by_stream`.
///
/// # Invariants
///
/// A saga id *is* its stream key, so it obeys the framework's
/// [reserved namespace](nitinol_persistence::reserved) law like every other
/// identifier: `nitinol` and anything beneath it are the framework's, and
/// naming one panics.  The law is owned by `nitinol-persistence`; this
/// constructor is one of its enforcement points, not a second definition.
///
/// `SagaId` is a distinct newtype — it is **not** interchangeable with
/// `AggregateId`.  The following must fail to compile:
///
/// ```compile_fail
/// use nitinol_saga::SagaId;
/// use nitinol_persistence::AggregateId;
/// let _: SagaId = AggregateId::new("same-text");
/// ```
///
/// ```compile_fail
/// use nitinol_saga::SagaId;
/// use nitinol_persistence::AggregateId;
/// let _ = AggregateId::new("same-text") == SagaId::new("same-text");
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SagaId(String);

impl SagaId {
    pub fn new(s: impl Into<String>) -> Self {
        let id = s.into();
        reject_reserved_id("saga id", &id);
        Self(id)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Borrow<str> for SagaId {
    fn borrow(&self) -> &str {
        &self.0
    }
}
