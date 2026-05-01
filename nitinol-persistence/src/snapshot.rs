use bytes::Bytes;
use chrono::{DateTime, Utc};

use crate::id::AggregateId;

#[derive(Debug, Clone)]
pub struct PersistedSnapshot {
    pub aggregate_id: AggregateId,
    pub sequence: u64,
    pub payload: Bytes,
    pub created_at: DateTime<Utc>,
}
