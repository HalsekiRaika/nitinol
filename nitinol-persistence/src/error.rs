use crate::id::AggregateId;

#[derive(Debug, thiserror::Error)]
pub enum AppendError {
    #[error("sequence conflict for aggregate {0:?}")]
    SequenceConflict(AggregateId),
    #[error("aggregate id mismatch: expected {expected:?}, got {actual:?}")]
    AggregateMismatch {
        expected: AggregateId,
        actual: AggregateId,
    },
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
