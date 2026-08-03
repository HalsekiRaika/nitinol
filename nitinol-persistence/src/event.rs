use bytes::Bytes;
use jiff::Timestamp;

use crate::event_type::EventType;

/// Append only. Does not carry `global_sequence` because it is assigned by the DB.
///
/// The stream key is supplied by [`crate::store::EventStore::append`]'s `key`
/// parameter; carrying it on the event itself would be a redundant second
/// source of truth.
#[derive(Debug, Clone)]
pub struct AppendingEvent {
    pub sequence: u64,
    pub event_type: EventType,
    pub payload: Bytes,
    pub occurred_at: Timestamp,
}

/// For loading. Includes `global_sequence`.
#[derive(Debug, Clone)]
pub struct LoadedEvent {
    /// The stream key under which this event was appended.  The same physical
    /// store may host aggregate streams and saga streams side by side, so the
    /// key is kept as a `String` rather than a typed `AggregateId`.
    pub stream_key: String,
    pub sequence: u64,
    pub global_sequence: u64,
    pub event_type: EventType,
    pub payload: Bytes,
    pub occurred_at: Timestamp,
}
