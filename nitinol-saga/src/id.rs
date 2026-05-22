//! Saga identifier.
//!
//! A saga instance is uniquely identified by an [`AggregateId`].  Aliased to
//! [`SagaId`] because, in the MVP, the underlying event store query path
//! (`LoadQuery::by_aggregate`) is reused verbatim for the saga's own event
//! stream — introducing a separate wrapper would only add a no-op conversion
//! at the persistence boundary.

use nitinol_persistence::AggregateId;

/// Identifier for a saga instance.
///
/// Type-aliased to [`AggregateId`] so that the saga's own events can be loaded
/// and appended through the existing `EventPersistor` API without any
/// additional conversion layer.
pub type SagaId = AggregateId;
