//! JSON codec used by the basic-aggregate example.
//!
//! Uses `serde_json` for both encoding and decoding.  A codec is a format
//! rather than a value — it is never constructed, only named, which is what
//! `#[event(codec = JsonCodec)]` on `Incremented` does.

use bytes::Bytes;
use serde::{Deserialize, Serialize};

use nitinol_eventsource::codec::Codec;

/// JSON codec: encodes events as JSON bytes and decodes them back.
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
