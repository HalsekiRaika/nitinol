//! Runtime context passed to [`crate::Saga::handle`].

use nitinol_persistence::AggregateId;

use crate::id::SagaId;

pub struct SagaContext {
    saga_id: SagaId,
    sequence: u64,
    upstream_aggregate_id: AggregateId,
    upstream_sequence: u64,
    now: jiff::Timestamp,
}

impl SagaContext {
    pub(crate) fn new(
        saga_id: SagaId,
        sequence: u64,
        upstream_aggregate_id: AggregateId,
        upstream_sequence: u64,
        now: jiff::Timestamp,
    ) -> Self {
        Self {
            saga_id,
            sequence,
            upstream_aggregate_id,
            upstream_sequence,
            now,
        }
    }

    pub fn test_context(saga_id: SagaId, sequence: u64) -> Self {
        Self {
            saga_id,
            sequence,
            upstream_aggregate_id: AggregateId::new(""),
            upstream_sequence: 0,
            now: jiff::Timestamp::UNIX_EPOCH,
        }
    }

    /// Acts as "set", not "merge": calling it twice keeps the most recent
    /// upstream values.
    pub fn with_upstream(
        mut self,
        upstream_aggregate_id: AggregateId,
        upstream_sequence: u64,
    ) -> Self {
        self.upstream_aggregate_id = upstream_aggregate_id;
        self.upstream_sequence = upstream_sequence;
        self
    }

    /// Acts as "set", not "merge": calling it twice keeps the most recent
    /// timestamp.
    pub fn with_now(mut self, now: jiff::Timestamp) -> Self {
        self.now = now;
        self
    }

    pub fn saga_id(&self) -> &SagaId {
        &self.saga_id
    }

    pub fn sequence(&self) -> u64 {
        self.sequence
    }

    pub fn upstream_aggregate_id(&self) -> &AggregateId {
        &self.upstream_aggregate_id
    }

    pub fn upstream_sequence(&self) -> u64 {
        self.upstream_sequence
    }

    pub fn now(&self) -> jiff::Timestamp {
        self.now
    }
}
