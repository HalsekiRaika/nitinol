use nitinol_persistence::AggregateId;

/// Runtime context passed to [`crate::Decider::decide`].
///
/// `Context` exposes the identity and current sequence position of the running
/// aggregate instance.  It is constructed by the runtime and should not be
/// created manually in application code.
pub struct Context {
    aggregate_id: AggregateId,
    sequence: u64,
}

impl Context {
    pub fn new(aggregate_id: AggregateId, sequence: u64) -> Self {
        Self {
            aggregate_id,
            sequence,
        }
    }

    /// Returns the identifier of the aggregate instance that owns this context.
    pub fn aggregate_id(&self) -> &AggregateId {
        &self.aggregate_id
    }

    /// Returns the last committed sequence number at the start of the current
    /// decision round.
    ///
    /// # Sequence number semantics
    ///
    /// During `Decider::decide`, `sequence()` reflects the sequence number of
    /// the most recently applied event **before** this decision round started.
    /// After each `Aggregate::apply` call the runtime increments the internal
    /// counter, so later decisions within the same round observe the
    /// updated sequence.  On a brand-new aggregate with no events the value
    /// is `0`.
    pub fn sequence(&self) -> u64 {
        self.sequence
    }
}
