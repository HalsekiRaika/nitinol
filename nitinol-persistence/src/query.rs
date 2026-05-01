use crate::event_type::EventType;
use crate::id::AggregateId;

#[derive(Debug, Clone, Default)]
pub struct LoadQuery {
    pub aggregate_id: Option<AggregateId>,
    pub event_type: Option<EventType>,
    pub from_global_sequence: Option<u64>,
    pub from_aggregate_sequence: Option<u64>,
    pub limit: Option<usize>,
}

impl LoadQuery {
    pub fn by_aggregate(id: AggregateId) -> Self {
        Self {
            aggregate_id: Some(id),
            ..Default::default()
        }
    }

    pub fn by_event_type(et: EventType) -> Self {
        Self {
            event_type: Some(et),
            ..Default::default()
        }
    }

    pub fn from_global(seq: u64) -> Self {
        Self {
            from_global_sequence: Some(seq),
            ..Default::default()
        }
    }

    pub fn with_limit(mut self, n: usize) -> Self {
        self.limit = Some(n);
        self
    }
}

#[derive(Debug, Clone)]
pub struct AppendOutcome {
    pub assigned_sequences: Vec<u64>,
    pub stream_version: u64,
}
