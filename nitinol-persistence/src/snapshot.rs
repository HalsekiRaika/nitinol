use bytes::Bytes;
use jiff::Timestamp;

use crate::id::AggregateId;

/// A snapshot as a snapshot store saves and loads it.
#[derive(Debug, Clone)]
pub struct PersistedSnapshot {
    /// The aggregate the state belongs to.
    pub aggregate_id: AggregateId,
    /// The position in the aggregate's stream the state was taken at.
    pub sequence: u64,
    /// The encoded state.
    pub payload: Bytes,
    /// When the snapshot was taken.
    pub created_at: Timestamp,
}
