use nitinol_persistence::AggregateId;

/// A typed event envelope carrying the aggregate identity, sequence numbers,
/// and the decoded event value.  Used as the message type for live-stream
/// subscriptions (`Stream<EventEnvelope<E>>`).
#[derive(Clone)]
pub struct EventEnvelope<E> {
    pub aggregate_id: AggregateId,
    /// Aggregate-scoped sequence number.
    pub sequence: u64,
    /// Global (cross-aggregate) sequence number assigned by the event store.
    pub global_sequence: u64,
    pub event: E,
}
