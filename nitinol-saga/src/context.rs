//! Runtime context passed to [`crate::Saga::handle`].

use crate::id::SagaId;

/// Runtime context exposed to a saga during `handle`.
///
/// Mirrors `nitinol_eventsource::Context` but is scoped to saga identity and
/// the saga's own event-stream sequence — the saga has its own event stream
/// and tracks its own `sequence` independently of the upstream aggregate that
/// produced the subscribed event.
pub struct SagaContext {
    saga_id: SagaId,
    sequence: u64,
}

impl SagaContext {
    /// Construct a new context.  The runtime calls this before each `handle`.
    pub(crate) fn new(saga_id: SagaId, sequence: u64) -> Self {
        Self { saga_id, sequence }
    }

    /// Returns the identifier of the running saga instance.
    pub fn saga_id(&self) -> &SagaId {
        &self.saga_id
    }

    /// Returns the last committed sequence of the saga's own event stream at
    /// the start of this `handle` invocation.
    ///
    /// A fresh saga that has not yet persisted any of its own events sees
    /// `sequence() == 0`.  Each [`crate::SagaEffect::Persist`] appended event
    /// advances this counter by one after a successful append.
    pub fn sequence(&self) -> u64 {
        self.sequence
    }
}
