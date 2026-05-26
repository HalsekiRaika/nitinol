#[derive(Debug, thiserror::Error)]
pub enum AppendError {
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
