use nitinol_persistence::AggregateId;

pub struct Context {
    aggregate_id: AggregateId,
    sequence: u64,
}

impl Context {
    pub fn new(aggregate_id: AggregateId, sequence: u64) -> Self {
        Self { aggregate_id, sequence }
    }

    pub fn aggregate_id(&self) -> &AggregateId {
        &self.aggregate_id
    }

    pub fn sequence(&self) -> u64 {
        self.sequence
    }
}
