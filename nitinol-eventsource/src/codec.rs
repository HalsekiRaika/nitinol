use bytes::Bytes;

use crate::error::CodecError;

/// User-facing codec trait for encoding and decoding event/snapshot types.
///
/// Both methods are associated functions (no `&self`) — codecs are stateless.
/// The framework promotes any `Codec<E>` to [`ErasedCodec<E>`] via the blanket
/// impl below so it can be stored as `Arc<dyn ErasedCodec<E>>`.
pub trait Codec<E>: Send + Sync + 'static {
    /// The error type produced when encoding or decoding fails.
    type Error: std::error::Error + Send + Sync + 'static;
    
    /// Encode `event` to raw bytes.
    fn encode(event: &E) -> Result<Bytes, Self::Error>;
    
    /// Decode an event from raw bytes.
    fn decode(payload: &[u8]) -> Result<E, Self::Error>;
}

/// Framework-internal, type-erased version of [`Codec<E>`].
///
/// Provides `&self` methods that wrap errors as [`CodecError`], allowing the
/// framework to store codecs as `Arc<dyn ErasedCodec<E>>` without knowing the
/// concrete codec type.
///
/// Do not implement this trait directly — the blanket impl below promotes any
/// [`Codec<E>`] automatically.
pub trait ErasedCodec<E>: Send + Sync + 'static {
    /// Encode `event` to raw bytes, mapping errors to [`CodecError::Encode`].
    fn encode(&self, event: &E) -> Result<Bytes, CodecError>;
    
    /// Decode an event from raw bytes, mapping errors to [`CodecError::Decode`].
    fn decode(&self, payload: &[u8]) -> Result<E, CodecError>;
}

/// Blanket impl: any [`Codec<E>`] is automatically an [`ErasedCodec<E>`].
impl<E, C> ErasedCodec<E> for C
where
    C: Codec<E>,
{
    fn encode(&self, event: &E) -> Result<Bytes, CodecError> {
        C::encode(event).map_err(|e| CodecError::Encode(Box::new(e)))
    }
    
    fn decode(&self, payload: &[u8]) -> Result<E, CodecError> {
        C::decode(payload).map_err(|e| CodecError::Decode(Box::new(e)))
    }
}
