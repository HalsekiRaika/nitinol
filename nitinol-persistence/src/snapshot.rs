use bytes::Bytes;
use jiff::Timestamp;

use crate::id::AggregateId;

#[derive(Debug, Clone)]
pub struct PersistedSnapshot {
    pub aggregate_id: AggregateId,
    pub sequence: u64,
    pub payload: Bytes,
    pub created_at: Timestamp,
}
