//! Multiple codec implementations demonstrating how to plug in different backends.
//!
//! This module defines:
//! - `JsonCodec`:    serde_json-backed, human-readable
//! - `PassThrough`:  no-op codec for unit-struct events (no payload)
//! - `UserDefinedCodec`: skeleton showing how to implement a custom codec
//!
//! Swapping codecs is a purely type-level change — the rest of the application
//! code stays the same.

use bytes::Bytes;
use serde::{Deserialize, Serialize};

use nitinol_eventsource::codec::Codec;

use crate::counter::Incremented;

// JsonCodec

/// JSON codec: encodes events as UTF-8 JSON bytes.
///
/// Works for any event type that derives `Serialize` + `Deserialize`.
#[derive(Default)]
pub struct JsonCodec;

impl<E: Serialize + for<'de> Deserialize<'de>> Codec<E> for JsonCodec {
    type Error = serde_json::Error;

    fn encode(event: &E) -> Result<Bytes, Self::Error> {
        serde_json::to_vec(event).map(Bytes::from)
    }

    fn decode(payload: &[u8]) -> Result<E, Self::Error> {
        serde_json::from_slice(payload)
    }
}

// PassThrough codec

/// Pass-through codec for unit-struct events that carry no data.
///
/// encode → always returns empty `Bytes`
/// decode → always returns the event value without reading the payload
///
/// Because `Incremented` is a unit struct, there is nothing to encode or decode.
#[derive(Default)]
pub struct PassThrough;

impl Codec<Incremented> for PassThrough {
    type Error = std::convert::Infallible;

    fn encode(_event: &Incremented) -> Result<Bytes, Self::Error> {
        Ok(Bytes::new())
    }

    fn decode(_payload: &[u8]) -> Result<Incremented, Self::Error> {
        Ok(Incremented)
    }
}

// UserDefinedCodec — skeleton for a custom format

/// Skeleton showing the minimum required to implement a custom codec.
///
/// Replace the encode/decode bodies with the real serialisation logic
/// (e.g. MessagePack, Avro, Protobuf …).
#[derive(Default)]
pub struct UserDefinedCodec;

#[derive(Debug, thiserror::Error)]
#[error("custom codec error: {0}")]
pub struct CustomCodecError(String);

impl Codec<Incremented> for UserDefinedCodec {
    type Error = CustomCodecError;

    fn encode(_event: &Incremented) -> Result<Bytes, Self::Error> {
        // Replace with your custom serialisation
        Ok(Bytes::from_static(b"INC"))
    }

    fn decode(payload: &[u8]) -> Result<Incremented, Self::Error> {
        // Replace with your custom deserialisation
        if payload == b"INC" {
            Ok(Incremented)
        } else {
            Err(CustomCodecError(format!(
                "unexpected payload: {:?}",
                payload
            )))
        }
    }
}
