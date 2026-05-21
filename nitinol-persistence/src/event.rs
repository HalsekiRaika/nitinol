use bytes::Bytes;
use jiff::Timestamp;

use crate::event_type::EventType;
use crate::id::AggregateId;

/// Append only. Does not carry `global_sequence` because it is assigned by the DB.
#[derive(Debug, Clone)]
pub struct AppendingEvent {
    pub aggregate_id: AggregateId,
    pub sequence: u64,
    pub event_type: EventType,
    pub payload: Bytes,
    pub occurred_at: Timestamp,
}

/// For loading. Includes `global_sequence`.
#[derive(Debug, Clone)]
pub struct LoadedEvent {
    pub aggregate_id: AggregateId,
    pub sequence: u64,
    pub global_sequence: u64,
    pub event_type: EventType,
    pub payload: Bytes,
    pub occurred_at: Timestamp,
}
