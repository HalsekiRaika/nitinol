//! What a delivery host is called, and the key each of its partitions keeps
//! its progress under.

use nitinol_persistence::store::CheckpointScope;
use nitinol_persistence::RESERVED_NAMESPACE;

/// Identifier of a delivery host — what its recorded progress is filed under.
///
/// A deployment names its own: two hosts over one saga type — a second
/// deployment, a migration running beside the first — keep separate progress,
/// and which of them a stored position belongs to is a decision the wiring
/// makes rather than one derived from the type. Deriving it from the saga's
/// declared event type is the default when the wiring names none.
///
/// # Invariants
///
/// None on the text. A delivery scope is kept apart from a projection's key by
/// the fixed prefix [`SagaDeliveryScope`] renders, so nothing about the
/// identifier's own content has to be refused — and a saga whose events belong
/// to a framework-owned family would have its derived default refused if it
/// were.
///
/// `SagaDeliveryId` is a distinct newtype — it is **not** interchangeable with
/// [`ProjectionId`](nitinol_persistence::ProjectionId), which names a different
/// kind of consumer. The following must fail to compile:
///
/// ```compile_fail
/// use nitinol_persistence::ProjectionId;
/// use nitinol_saga::SagaDeliveryId;
/// let _: SagaDeliveryId = ProjectionId::new("same-text");
/// ```
///
/// ```compile_fail
/// use nitinol_persistence::ProjectionId;
/// use nitinol_saga::SagaDeliveryId;
/// let _ = ProjectionId::new("same-text") == SagaDeliveryId::new("same-text");
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SagaDeliveryId(String);

impl SagaDeliveryId {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The checkpoint key of one partition of one delivery host.
///
/// Progress is per partition rather than per host: a host recording one
/// position for all its partitions would have them overwrite each other, and a
/// held partition would drag the rest back to its own position on restart. The
/// key is therefore the identifier together with the partition number.
///
/// # Why the key cannot collide with a projection's
///
/// The rendering begins with the framework's own
/// [reserved namespace](nitinol_persistence::reserved), which every
/// user-facing identifier constructor refuses while it is being constructed. A
/// [`ProjectionId`](nitinol_persistence::ProjectionId) that rendered the same
/// text as a scope below therefore cannot exist, so the two key spaces are
/// disjoint by construction and one provider table serves both.
///
/// # Why the rendering is injective
///
/// The partition number is decimal and comes last, so splitting at the final
/// separator recovers the pair. An identifier that itself ends in something
/// readable as a partition number does not collide with the pair that would
/// render the same text, because that pair's number would then be the
/// identifier's own trailing part and the identifier the shorter prefix.
pub struct SagaDeliveryScope {
    rendered: String,
}

impl SagaDeliveryScope {
    /// The key partition `partition` of the host named `id` records under.
    pub fn new(id: &SagaDeliveryId, partition: usize) -> Self {
        Self {
            rendered: format!(
                "{RESERVED_NAMESPACE}.saga.delivery.{}.{partition}",
                id.as_str()
            ),
        }
    }
}

impl CheckpointScope for SagaDeliveryScope {
    fn checkpoint_key(&self) -> &str {
        &self.rendered
    }
}
