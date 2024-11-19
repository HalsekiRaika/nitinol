use std::error::Error;
use std::fmt::{Display, Formatter};

#[cfg(feature = "default")]
#[derive(Debug, thiserror::Error)]
pub enum ProjectionError {
    #[error("Errors occurred in external protocols.")]
    Protocol(#[source] Box<dyn Error + Sync + Send>),
    #[error("Failed projection")]
    Projection(#[from] Box<dyn Error + Sync + Send>),
    #[error(transparent)]
    Serde(Box<dyn Error + Sync + Send>),
}

#[cfg(feature = "default")]
impl From<SerializeError> for ProjectionError {
    fn from(value: SerializeError) -> Self {
        Self::Serde(value.0)
    }
}

#[cfg(feature = "default")]
impl From<DeserializeError> for ProjectionError {
    fn from(value: DeserializeError) -> Self {
        Self::Serde(value.0)
    }
}

#[cfg(feature = "default")]
#[derive(Debug)]
pub(crate) struct UnimplementedError;


#[cfg(feature = "default")]
impl Display for UnimplementedError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "Unimplemented")
    }
}

#[cfg(feature = "default")]
impl Error for UnimplementedError {}


#[cfg(feature = "markers")]
#[derive(Debug)]
pub struct SerializeError(Box<dyn Error + Sync + Send>);

#[cfg(feature = "markers")]
impl<E: serde::ser::Error + Sync + Send + 'static> From<E> for SerializeError {
    fn from(value: E) -> Self {
        Self(Box::new(value))
    }
}

#[cfg(feature = "markers")]
impl Display for SerializeError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "Failed to serialize event: {}", self.0)
    }
}

#[cfg(feature = "markers")]
impl Error for SerializeError {}

#[cfg(feature = "markers")]
#[derive(Debug)]
pub struct DeserializeError(Box<dyn Error + Sync + Send>);

#[cfg(feature = "markers")]
impl<E: serde::de::Error + Sync + Send + 'static> From<E> for DeserializeError {
    fn from(value: E) -> Self {
        Self(Box::new(value))
    }
}

#[cfg(feature = "markers")]
impl Display for DeserializeError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "Failed to deserialize event: {}", self.0)
    }
}

#[cfg(feature = "markers")]
impl Error for DeserializeError {}
