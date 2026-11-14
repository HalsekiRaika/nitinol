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
/// - A deployment names its own, so two hosts over one saga type (a second
///   deployment, a migration running beside the first) keep separate
///   progress. When the wiring names none, the default is derived from the
///   saga's declared event type.
/// - The text carries no constraint: [`OffsetScope::framework_owned`] renders
///   the key inside the framework's own namespace whatever it contains.
/// - A distinct newtype: it does not convert to or compare with
///   [`ProjectionId`](nitinol_eventsource::ProjectionId), which names a
///   different kind of consumer.
///
/// ```compile_fail
/// use nitinol_eventsource::ProjectionId;
/// use nitinol_saga::SagaDeliveryId;
/// let _: SagaDeliveryId = ProjectionId::new("same-text");
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SagaDeliveryId(String);

impl SagaDeliveryId {
    /// Creates a delivery-host identifier from a string.
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    /// Returns the identifier as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The key partition `partition` of this host records its progress under.
    ///
    /// Progress is per partition rather than per host: a host recording one
    /// offset for all its partitions would have them overwrite each other,
    /// and a held partition would drag the rest back to its own offset on
    /// restart. The key is therefore this identifier together with the
    /// partition number, and that the two render one key injectively — inside
    /// the framework's own namespace, where no user-named scope can reach — is
    /// [`OffsetScope`]'s obligation rather than this type's.
    pub fn offset_scope(&self, partition: usize) -> OffsetScope {
        OffsetScope::framework_owned(SAGA_DELIVERY_PART, self.as_str(), partition)
    }
}

#[cfg(doctest)]
mod compile_fail_fixtures {
    /// ```compile_fail
    /// use nitinol_eventsource::ProjectionId;
    /// use nitinol_saga::SagaDeliveryId;
    /// let _ = ProjectionId::new("same-text") == SagaDeliveryId::new("same-text");
    /// ```
    fn saga_delivery_id_does_not_compare_with_projection_id() {}
}
