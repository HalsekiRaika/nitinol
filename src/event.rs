use serde::de::DeserializeOwned;
use serde::Serialize;
use crate::errors::{DeserializeError, SerializeError};

pub trait Event: 'static + Sync + Send + Sized 
    where Self: Serialize + DeserializeOwned
{
    const REGISTRY_KEY: &'static str;
    fn as_bytes(&self) -> Result<Vec<u8>, SerializeError>;
    fn from_bytes(bytes: &[u8]) -> Result<Self, DeserializeError>;
}
