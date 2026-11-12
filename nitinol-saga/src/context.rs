//! Runtime context passed to [`crate::Saga::handle`].

use nitinol_persistence::AggregateId;

use crate::id::SagaId;

/// What [`Saga::handle`](crate::Saga::handle) is told about the record it is
/// handling: which instance is handling it, how far that instance's own stream
/// has reached, where the record came from, and when.
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

    /// Creates a context for calling [`Saga::handle`](crate::Saga::handle)
    /// outside a running saga, such as in a unit test.
    ///
    /// Its upstream record is the empty id at sequence `0` and its
    /// [`now`](Self::now) is the Unix epoch; set them with
    /// [`with_upstream`](Self::with_upstream) and [`with_now`](Self::with_now).
    pub fn test_context(saga_id: SagaId, sequence: u64) -> Self {
        Self {
            saga_id,
            sequence,
            upstream_aggregate_id: AggregateId::new(""),
            upstream_sequence: 0,
            now: jiff::Timestamp::UNIX_EPOCH,
        }
    }

    /// Sets the upstream record to `upstream_aggregate_id` at
    /// `upstream_sequence`.
    ///
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

    /// Sets the time [`now`](Self::now) returns.
    ///
    /// Acts as "set", not "merge": calling it twice keeps the most recent
    /// timestamp.
    pub fn with_now(mut self, now: jiff::Timestamp) -> Self {
        self.now = now;
        self
    }

    /// Returns the id of the saga instance handling the record.
    pub fn saga_id(&self) -> &SagaId {
        &self.saga_id
    }

    /// Returns the sequence this saga's own stream has reached.
    pub fn sequence(&self) -> u64 {
        self.sequence
    }

    /// Returns the stream key of the observed record; empty for a deadline
    /// firing.
    pub fn upstream_aggregate_id(&self) -> &AggregateId {
        &self.upstream_aggregate_id
    }

    /// Returns the sequence of the observed record in its stream; `0` for a
    /// deadline firing.
    pub fn upstream_sequence(&self) -> u64 {
        self.upstream_sequence
    }

    /// Returns the time at which this `handle` call was made.
    pub fn now(&self) -> jiff::Timestamp {
        self.now
    }
}
