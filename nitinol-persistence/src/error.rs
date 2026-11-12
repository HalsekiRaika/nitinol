//! Errors returned by the event, snapshot and offset stores.

use nitinol_contract::EventType;

/// The reason an append to an event store failed.
#[derive(Debug, thiserror::Error)]
pub enum AppendError {
    /// A double append to an already-taken `(stream, sequence)` pair was
    /// detected and rejected.  The payload is the stream key.
    ///
    /// This is a detection result, not a transient failure: the store does not
    /// try to resolve the conflict internally, the events already stored are
    /// untouched, and replaying the identical append conflicts again.
    ///
    /// On the creation path the variant is a signal rather than a fault.  A
    /// conflict on a stream's genesis sequence means the aggregate has already
    /// been created, so a creation command redelivered under at-least-once
    /// semantics may treat it as a successful response.
    ///
    /// See [`EventStore::append`](crate::store::EventStore::append) for the
    /// full OCC contract.
    #[error("sequence conflict for stream {0:?}")]
    SequenceConflict(String),
    /// An error returned by the backend.
    #[error("backend failure: {0}")]
    Backend(Box<dyn std::error::Error + Send + Sync>),
}

/// The reason a load from an event store, or one row of a typed read, failed.
#[derive(Debug, thiserror::Error)]
pub enum LoadError {
    /// An error returned by the backend.
    #[error("backend failure: {0}")]
    Backend(Box<dyn std::error::Error + Send + Sync>),
    /// A stored row of `event_type` that the event type's codec could not read.
    ///
    /// The failure belongs to that one row: a typed read reports it in the
    /// row's place and goes on with the rows after it.
    #[error(
        "the payload of the {event_type} row at global position {global_sequence} \
         in stream {stream_key:?} could not be decoded: {source}"
    )]
    Decode {
        /// The event type the row was stored as.
        event_type: EventType,
        /// The stream the row belongs to.
        stream_key: String,
        /// The row's position across the store.
        global_sequence: u64,
        /// The error the codec returned.
        source: Box<dyn std::error::Error + Send + Sync>,
    },
}

/// The reason a snapshot store failed.
#[derive(Debug, thiserror::Error)]
pub enum SnapshotError {
    /// An error returned by the backend.
    #[error("backend failure: {0}")]
    Backend(Box<dyn std::error::Error + Send + Sync>),
}

/// The reason an offset store failed.
#[derive(Debug, thiserror::Error)]
pub enum OffsetError {
    /// An error returned by the backend.
    #[error("backend failure: {0}")]
    Backend(Box<dyn std::error::Error + Send + Sync>),
}
