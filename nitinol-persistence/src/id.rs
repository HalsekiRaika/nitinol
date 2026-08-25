use std::borrow::Borrow;

use crate::reserved::reject_reserved_id;

/// Identifier of an aggregate — and, verbatim, the key of the EventStore stream
/// it persists to.
///
/// # Invariants
///
/// The name must lie outside the [reserved namespace](crate::reserved):
/// `nitinol` and anything beneath it belong to the framework.  Violating this
/// panics, the same way a malformed [`Family`](crate::Family) does — a reserved
/// key can only come from a literal or a configuration value, never from data
/// the framework produced.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct AggregateId(String);

impl AggregateId {
    pub fn new(s: impl Into<String>) -> Self {
        let id = s.into();
        reject_reserved_id("aggregate id", &id);
        Self(id)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Borrow<str> for AggregateId {
    fn borrow(&self) -> &str {
        &self.0
    }
}

/// Identifier of a projection, used as its checkpoint key.
///
/// # Invariants
///
/// The same reserved-namespace rule as [`AggregateId`] applies.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ProjectionId(String);

impl ProjectionId {
    pub fn new(s: impl Into<String>) -> Self {
        let id = s.into();
        reject_reserved_id("projection id", &id);
        Self(id)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}
