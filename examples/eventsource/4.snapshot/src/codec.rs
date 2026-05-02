//! Codecs for the snapshot example.
//!
//! - `EventCodec`: pass-through codec for `Incremented` (no payload needed)
//! - `SnapshotCodec`: big-endian u64 encoding for the counter snapshot

use bytes::Bytes;

use nitinol_eventsource::codec::Codec;

use crate::counter::Incremented;

/// Pass-through codec: `Incremented` carries no data, so encode/decode are no-ops.
#[derive(Default)]
pub struct EventCodec;

impl Codec<Incremented> for EventCodec {
    type Error = std::convert::Infallible;

    fn encode(_event: &Incremented) -> Result<Bytes, Self::Error> {
        Ok(Bytes::new())
    }

    fn decode(_payload: &[u8]) -> Result<Incremented, Self::Error> {
        Ok(Incremented)
    }
}

/// Big-endian u64 codec for counter snapshots.
#[derive(Default)]
pub struct SnapshotCodec;

#[derive(Debug, thiserror::Error)]
#[error("snapshot payload must be 8 bytes, got {0}")]
pub struct SnapshotDecodeError(usize);

impl Codec<u64> for SnapshotCodec {
    type Error = SnapshotDecodeError;

    fn encode(value: &u64) -> Result<Bytes, Self::Error> {
        Ok(Bytes::from(value.to_be_bytes().to_vec()))
    }

    fn decode(payload: &[u8]) -> Result<u64, Self::Error> {
        payload
            .try_into()
            .map(u64::from_be_bytes)
            .map_err(|_| SnapshotDecodeError(payload.len()))
    }
}
