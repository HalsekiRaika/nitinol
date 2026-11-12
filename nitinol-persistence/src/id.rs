use std::borrow::Borrow;

use crate::reserved::reject_reserved_id;

/// Identifier of an aggregate — and, verbatim, the key of the EventStore stream
/// it persists to.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct AggregateId(String);

impl AggregateId {
    /// Creates an aggregate id from a string.
    ///
    /// # Panics
    ///
    /// Panics if `s` is within the [reserved namespace](crate::reserved):
    /// `nitinol` itself or any name beneath it, such as `nitinol.saga`.  That
    /// is the same way a malformed [`Family`](crate::Family) panics — a
    /// reserved key can only come from a literal or a configuration value,
    /// never from data the framework produced.
    pub fn new(s: impl Into<String>) -> Self {
        let id = s.into();
        reject_reserved_id("aggregate id", &id);
        Self(id)
    }

    /// Returns the id as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Borrow<str> for AggregateId {
    fn borrow(&self) -> &str {
        &self.0
    }
}
