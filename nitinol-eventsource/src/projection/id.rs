use nitinol_persistence::reject_reserved_id;
use nitinol_persistence::store::OffsetScope;

/// Identifier of a projection — the name a deployment runs one under, and
/// verbatim the key its offset is filed under.
///
/// # Invariants
///
/// The name must lie outside the
/// [reserved namespace](nitinol_persistence::reserved): `nitinol` and anything
/// beneath it belong to the framework's own consumers. Violating this panics,
/// the same way [`AggregateId::new`](nitinol_persistence::AggregateId::new) and
/// `nitinol_saga::SagaId::new` do — a reserved key can only come from a literal
/// or a configuration value, never from data the framework produced.
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
