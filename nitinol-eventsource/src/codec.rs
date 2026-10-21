//! Calling the codec a domain type named, and classifying what it says.
//!
//! The trait itself lives in [`nitinol_contract`], next to the
//! [`Event`](nitinol_contract::Event) and
//! [`Snapshotable`](nitinol_contract::Snapshotable) traits that name one, and is
//! re-exported here because this crate's API is stated in terms of it and this
//! is the path downstream code already imports.
//!
//! What this module adds is the interpreter's half. A failure of a codec is one
//! of the ways an append or a replay can fail, so it is classified into
//! [`CodecError`] here rather than surfacing the domain's own error type through
//! the framework's — and it is classified in one place, so every caller reports
//! the same failure the same way.

use bytes::Bytes;

use crate::error::CodecError;

pub use nitinol_contract::Codec;

/// Encode `value` through `C` — the codec its type named — reporting a failure
/// of it as [`CodecError::Encode`].
pub fn encode<C, T>(value: &T) -> Result<Bytes, CodecError>
where
    C: Codec<T>,
{
    C::encode(value).map_err(|e| CodecError::Encode(Box::new(e)))
}

/// Decode a value from `payload` through `C` — the codec its type named —
/// reporting a failure of it as [`CodecError::Decode`].
pub fn decode<C, T>(payload: &[u8]) -> Result<T, CodecError>
where
    C: Codec<T>,
{
    C::decode(payload).map_err(|e| CodecError::Decode(Box::new(e)))
}
