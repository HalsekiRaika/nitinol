//! Runtime context passed to [`crate::Saga::handle`].

use nitinol_persistence::AggregateId;

use crate::id::SagaId;

pub struct SagaContext {
    saga_id: SagaId,
    sequence: u64,
    upstream_aggregate_id: AggregateId,
    upstream_sequence: u64,
    now: jiff::Timestamp,
    /// `tell_id`s whose outbox executors appended `TellFailed` since the last
    /// `handle` call, or whose `TellFailed` marker was seen during replay on
    /// restart.  The saga inspects this slice to detect unrecoverable tell
    /// failures and trigger compensation.
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

    /// `tell_id`s whose executor appended `TellFailed` since the last `handle`
    /// call, or whose `TellFailed` marker was detected during replay on restart.
    ///
    /// The saga reads this slice to detect unrecoverable tell failures and
    /// decide whether to trigger compensation.  The slice is drained every time
    /// `handle` is invoked — successive calls will not see the same `tell_id`
    /// twice.
    pub fn failed_tell_ids(&self) -> &[u64] {
        &self.failed_tell_ids
    }
}
