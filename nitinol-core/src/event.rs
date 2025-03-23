use crate::errors::{DeserializeError, SerializeError};

/// Trait representing the event.
/// 
/// ### Why must constants and (de)serialization be implemented?
/// Events are generally not used in the form of `Box<dyn Event>`, etc., 
/// because it is more convenient to convert them into portable data and use them around, 
/// as events are often passed to the PubSub network 
/// or persisted in a database for aggregates restoration.
pub trait Event: 'static + Sync + Send + Sized {
    const EVENT_TYPE: &'static str;
    fn as_bytes(&self) -> Result<Vec<u8>, SerializeError>;
    fn from_bytes(bytes: &[u8]) -> Result<Self, DeserializeError>;
}
