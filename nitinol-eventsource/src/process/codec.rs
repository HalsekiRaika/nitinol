use bytes::Bytes;
use nitinol_persistence::EventType;

/// Error produced when encoding an event to bytes.
#[derive(Debug, thiserror::Error)]
#[error("encode error: {0}")]
pub struct EncodeError(pub Box<dyn std::error::Error + Send + Sync>);

/// Error produced when decoding bytes to an event.
#[derive(Debug, thiserror::Error)]
#[error("decode error: {0}")]
pub struct DecodeError(pub Box<dyn std::error::Error + Send + Sync>);

/// Encodes and decodes a concrete event type `E` to and from raw bytes.
///
/// Implementors are responsible for choosing a serialization format.
/// The codec is stateless and object-safe so it can be stored as
/// `Arc<dyn EventCodec<E>>`.
pub trait EventCodec<E>: Send + Sync + 'static {
    fn encode(&self, event: &E) -> Result<Bytes, EncodeError>;
    fn decode(&self, event_type: EventType, bytes: Bytes) -> Result<E, DecodeError>;
}
