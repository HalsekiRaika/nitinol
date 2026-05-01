use bytes::Bytes;
use chrono::{DateTime, Utc};

use crate::id::AggregateId;
use crate::event_type::EventType;

/// append 用。global_sequence は DB が採番するため持たない。
#[derive(Debug, Clone)]
pub struct AppendingEvent {
    pub aggregate_id: AggregateId,
    pub sequence: u64,
    pub event_type: EventType,
    pub payload: Bytes,
    pub occurred_at: DateTime<Utc>,
}

/// load 用。global_sequence を含む。
#[derive(Debug, Clone)]
pub struct LoadedEvent {
    pub aggregate_id: AggregateId,
    pub sequence: u64,
    pub global_sequence: u64,
    pub event_type: EventType,
    pub payload: Bytes,
    pub occurred_at: DateTime<Utc>,
}
