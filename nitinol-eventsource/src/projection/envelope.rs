use nitinol_persistence::AggregateId;

/// A typed event envelope carrying the aggregate identity, sequence numbers,
/// and the decoded event value.
///
/// Produced by `DurableStream<EventEnvelope<E>>` transforms (e.g. the saga's
/// internal upstream subscription) and accepted by `Effect::publish` when an
/// aggregate wants to broadcast a typed event to a `Stream<EventEnvelope<E>>`.
#[derive(Clone)]
pub struct EventEnvelope<E> {
    pub aggregate_id: AggregateId,
    /// Aggregate-scoped sequence number.
    pub sequence: u64,
    /// Global (cross-aggregate) sequence number assigned by the event store.
    pub global_sequence: u64,
    pub event: E,
}
