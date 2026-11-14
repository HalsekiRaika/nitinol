use nitinol_persistence::reject_reserved_id;
use nitinol_persistence::store::OffsetScope;

/// Identifier of a projection — the name a deployment runs one under, and
/// verbatim the key its offset is filed under.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ProjectionId(String);

impl ProjectionId {
    /// Creates a projection id from a string.
    ///
    /// # Panics
    ///
    /// Panics if `s` lies within the
    /// [reserved namespace](nitinol_persistence::reserved): `nitinol` and
    /// anything beneath it belong to the framework's own consumers. This is the
    /// same refusal [`AggregateId::new`](nitinol_persistence::AggregateId::new)
    /// and `nitinol_saga::SagaId::new` make — a reserved key can only come from
    /// a literal or a configuration value, never from data the framework
    /// produced.
    pub fn new(s: impl Into<String>) -> Self {
        let id = s.into();
        reject_reserved_id("projection id", &id);
        Self(id)
    }

    /// Returns the id as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A projection names its own offset key, with nothing added to it.
///
/// Decorating the key — a prefix naming the kind, say — would move every row a
/// projection had already written, so this is the user-named constructor and not
/// a framework-owned one. The identifier has already been refused a reserved
/// name, which is the same refusal
/// [`OffsetScope::new`](nitinol_persistence::store::OffsetScope::new) makes.
impl From<&ProjectionId> for OffsetScope {
    fn from(id: &ProjectionId) -> Self {
        OffsetScope::new(id.as_str())
    }
}
