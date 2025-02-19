#[derive(Debug, thiserror::Error)]
pub enum ResolveError {
    #[error(transparent)]
    Deserialize(#[from] nitinol_core::errors::DeserializeError),

    #[error("An error occurred while applying the event. {trace}")]
    InProcess { trace: String },
}
