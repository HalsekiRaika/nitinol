use bytes::Bytes;
use jiff::Timestamp;
use nitinol_contract::EventType;

/// Append only. Does not carry `global_sequence` because it is assigned by the DB.
///
/// The stream key is supplied by [`crate::store::EventStore::append`]'s `key`
/// parameter; carrying it on the event itself would be a redundant second
/// source of truth.
#[derive(Debug, Clone)]
pub struct AppendingEvent {
    /// The event's position in its stream; a stream's first event is `1`.
    pub sequence: u64,
    /// The type the event is stored as.
    pub event_type: EventType,
    /// The encoded event.
    pub payload: Bytes,
    /// When the event occurred.
    pub occurred_at: Timestamp,
}

/// For loading. Includes `global_sequence`.
#[derive(Debug, Clone)]
pub struct LoadedEvent {
    /// The stream key under which this event was appended.  The same physical
    /// store may host aggregate streams and saga streams side by side, so the
    /// key is kept as a `String` rather than a typed `AggregateId`.
    pub stream_key: String,
    /// The event's position in its stream.
    pub sequence: u64,
    /// The event's position across the whole store.
    pub global_sequence: u64,
    /// The type the event was stored as.
    pub event_type: EventType,
    /// The encoded event.
    pub payload: Bytes,
    /// When the event occurred.
    pub occurred_at: Timestamp,
}
