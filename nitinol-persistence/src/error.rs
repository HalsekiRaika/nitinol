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
    #[error("backend failure: {0}")]
    Backend(Box<dyn std::error::Error + Send + Sync>),
}

#[derive(Debug, thiserror::Error)]
pub enum LoadError {
    #[error("backend failure: {0}")]
    Backend(Box<dyn std::error::Error + Send + Sync>),
}

#[derive(Debug, thiserror::Error)]
pub enum SnapshotError {
    #[error("backend failure: {0}")]
    Backend(Box<dyn std::error::Error + Send + Sync>),
}

#[derive(Debug, thiserror::Error)]
pub enum CheckpointError {
    #[error("backend failure: {0}")]
    Backend(Box<dyn std::error::Error + Send + Sync>),
}
