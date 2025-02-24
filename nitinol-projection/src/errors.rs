use nitinol_resolver::errors::ResolveError;

#[derive(Debug, thiserror::Error)]
pub enum ProjectionError {
    #[error("Failed to read protocol. {0}")]
    Protocol(#[from] nitinol_protocol::errors::ProtocolError),

    #[error(transparent)]
    NotCompatible(#[from] NotCompatible),

    #[error("First formation is not implemented.")]
    FirstFormation,

    #[error(transparent)]
    DeserializeEvent(#[from] nitinol_core::errors::DeserializeError),

    #[error("An error occurred while applying the event. {backtrace}")]
    ApplyEvent { backtrace: String },
}

#[derive(Debug, thiserror::Error)]
pub enum RejectProjection {
    #[error("Projection interrupted. This is a user defined behavior.")]
    Interrupted,
}

#[derive(Debug, thiserror::Error)]
#[error("There are data incompatible with Mapping. key:{key}")]
pub struct NotCompatible {
    pub key: String,
}

impl From<ResolveError> for ProjectionError {
    fn from(value: ResolveError) -> Self {
        match value {
            ResolveError::Deserialize(e) => Self::DeserializeEvent(e),
            ResolveError::InProcess { trace } => Self::ApplyEvent { backtrace: trace },
        }
    }
}
