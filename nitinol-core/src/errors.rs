use std::error::Error;

#[derive(Debug, thiserror::Error)]
#[error("Failed to serialize event: {0}")]
pub struct SerializeError(Box<dyn Error + Sync + Send>);

impl<E: serde::ser::Error + Sync + Send + 'static> From<E> for SerializeError {
    fn from(value: E) -> Self {
        Self(Box::new(value))
    }
}

#[derive(Debug, thiserror::Error)]
#[error("Failed to deserialize event: {0}")]
pub struct DeserializeError(Box<dyn Error + Sync + Send>);

impl<E: serde::de::Error + Sync + Send + 'static> From<E> for DeserializeError {
    fn from(value: E) -> Self {
        Self(Box::new(value))
    }
}
