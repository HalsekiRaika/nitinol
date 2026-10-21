//! What a delivery host is called, and the key each of its partitions keeps
//! its progress under.

use nitinol_persistence::store::OffsetScope;

/// The part of the framework a delivery host's offsets belong to.
///
/// Carries no `.`, which is what
/// [`OffsetScope::framework_owned`] asks of a part name.
const SAGA_DELIVERY_PART: &str = "saga-delivery";

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
/// None on the text. The key is rendered by
/// [`OffsetScope::framework_owned`], which puts it inside the framework's own
/// namespace whatever the identifier contains, so nothing about the
/// identifier's own content has to be refused — and a saga whose events belong
/// to a framework-owned family would have its derived default refused if it
/// were.
///
/// `SagaDeliveryId` is a distinct newtype — it is **not** interchangeable with
/// [`ProjectionId`](nitinol_eventsource::ProjectionId), which names a different
/// kind of consumer. The following must fail to compile:
///
/// ```compile_fail
/// use nitinol_eventsource::ProjectionId;
/// use nitinol_saga::SagaDeliveryId;
/// let _: SagaDeliveryId = ProjectionId::new("same-text");
/// ```
///
/// ```compile_fail
/// use nitinol_eventsource::ProjectionId;
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

    /// The key partition `partition` of this host records its progress under.
    ///
    /// Progress is per partition rather than per host: a host recording one
    /// position for all its partitions would have them overwrite each other,
    /// and a held partition would drag the rest back to its own position on
    /// restart. The key is therefore this identifier together with the
    /// partition number, and that the two render one key injectively — inside
    /// the framework's own namespace, where no user-named scope can reach — is
    /// [`OffsetScope`]'s obligation rather than this type's.
    pub fn offset_scope(&self, partition: usize) -> OffsetScope {
        OffsetScope::framework_owned(SAGA_DELIVERY_PART, self.as_str(), partition)
    }
}
