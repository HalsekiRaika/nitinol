use bytes::Bytes;
use jiff::Timestamp;

use crate::event_type::EventType;
use crate::id::AggregateId;

/// append 用。global_sequence は DB が採番するため持たない。
#[derive(Debug, Clone)]
pub struct AppendingEvent {
    pub aggregate_id: AggregateId,
    pub sequence: u64,
    pub event_type: EventType,
    pub payload: Bytes,
    pub occurred_at: Timestamp,
}

/// load 用。global_sequence を含む。
#[derive(Debug, Clone)]
pub struct LoadedEvent {
    pub aggregate_id: AggregateId,
    pub sequence: u64,
    pub global_sequence: u64,
    pub event_type: EventType,
    pub payload: Bytes,
    pub occurred_at: Timestamp,
}
