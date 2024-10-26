use std::error::Error;
use std::fmt::{Display, Formatter};

#[derive(Debug, thiserror::Error)]
pub enum ProjectionError {
    #[error("Failed projection")]
    Projection,
    #[error(transparent)]
    Serde(Box<dyn Error + Sync + Send>)
}

impl From<SerializeError> for ProjectionError {
    fn from(value: SerializeError) -> Self {
        Self::Serde(value.0)
    }
}

impl From<DeserializeError> for ProjectionError {
    fn from(value: DeserializeError) -> Self {
        Self::Serde(value.0)
    }
}

#[derive(Debug)]
pub struct SerializeError(Box<dyn Error + Sync + Send>);

impl<E: serde::ser::Error + Sync + Send + 'static> From<E> for SerializeError {
    fn from(value: E) -> Self {
        Self(Box::new(value))
    }
}

impl Display for SerializeError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "Failed to serialize event: {}", self.0)
    }
}

impl Error for SerializeError {}

#[derive(Debug)]
pub struct DeserializeError(Box<dyn Error + Sync + Send>);

impl<E: serde::de::Error + Sync + Send + 'static> From<E> for DeserializeError {
    fn from(value: E) -> Self {
        Self(Box::new(value))
    }
}

impl Display for DeserializeError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "Failed to deserialize event: {}", self.0)
    }
}

impl Error for DeserializeError {}
