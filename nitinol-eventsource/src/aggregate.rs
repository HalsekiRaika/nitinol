use bytes::Bytes;

use crate::event::Event;

pub trait Aggregate: Default + Send + Sync + 'static {
    type Event: Event;

    fn apply(&mut self, event: Self::Event);
}

pub trait Snapshotable: Sized {
    fn restore(payload: &[u8]) -> Result<Self, SnapshotRestoreError>;
    fn capture(&self) -> Result<Bytes, SnapshotCaptureError>;
}

#[derive(Debug, thiserror::Error)]
pub enum SnapshotRestoreError {
    #[error("decode error: {0}")]
    Decode(Box<dyn std::error::Error + Send + Sync>),
}

#[derive(Debug, thiserror::Error)]
pub enum SnapshotCaptureError {
    #[error("encode error: {0}")]
    Encode(Box<dyn std::error::Error + Send + Sync>),
}
