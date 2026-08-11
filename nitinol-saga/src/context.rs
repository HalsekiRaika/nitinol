//! Runtime context passed to [`crate::Saga::handle`].

use nitinol_persistence::AggregateId;

use crate::id::SagaId;

pub struct SagaContext {
    saga_id: SagaId,
    sequence: u64,
    upstream_aggregate_id: AggregateId,
    upstream_sequence: u64,
    now: jiff::Timestamp,
    /// `tell_id`s of unrecoverable tell failures this invocation is told about.
    /// Which failures land here depends on the entry point: see
    /// [`SagaContext::failed_tell_ids`].
    failed_tell_ids: Vec<u64>,
}

impl SagaContext {
    pub(crate) fn new(
        saga_id: SagaId,
        sequence: u64,
        upstream_aggregate_id: AggregateId,
        upstream_sequence: u64,
        now: jiff::Timestamp,
        failed_tell_ids: Vec<u64>,
    ) -> Self {
        Self {
            saga_id,
            sequence,
            upstream_aggregate_id,
            upstream_sequence,
            now,
            failed_tell_ids,
        }
    }

    pub fn test_context(saga_id: SagaId, sequence: u64) -> Self {
        Self {
            saga_id,
            sequence,
            upstream_aggregate_id: AggregateId::new(""),
            upstream_sequence: 0,
            now: jiff::Timestamp::UNIX_EPOCH,
            failed_tell_ids: Vec::new(),
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

    /// `tell_id`s of unrecoverable tell failures the saga has not yet been told
    /// about.
    ///
    /// In [`crate::Saga::handle`] and [`crate::Saga::on_scheduled`] this carries
    /// the failures recovered by replay on restart — `TellFailed` markers found
    /// on the saga's own stream, and the synthetic ones written for tells that
    /// could not be re-dispatched.  A failure that settles while the saga is
    /// running goes to [`crate::Saga::on_tell_failed`] the moment it happens
    /// and does **not** reappear here, so the same failure is never announced
    /// twice.
    ///
    /// In [`crate::Saga::on_tell_failed`] this carries exactly the one
    /// `tell_id` that invocation is about, which is how a saga with several
    /// outstanding tells tells them apart.
    ///
    /// The slice is drained on every invocation — successive calls will not see
    /// the same `tell_id` twice.
    pub fn failed_tell_ids(&self) -> &[u64] {
        &self.failed_tell_ids
    }
}
