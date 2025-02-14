use std::error::Error;
use std::fmt::{Display, Formatter};


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
