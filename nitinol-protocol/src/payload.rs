use std::cmp::Ordering;

/// Basic format of the data to be saved.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "sqlx", derive(sqlx::FromRow))]
pub struct Payload {
    /// Unique sequence value at a specific Entity
    pub sequence_id: i64,
    /// Unique id for each data format used in [`ResolveMapping`](spectroscopy_core::mapping::ResolveMapping)
    pub registry_key: String,
    /// Data body in binary format
    pub bytes: Vec<u8>,
}

impl Eq for Payload {}

impl PartialEq<Self> for Payload {
    fn eq(&self, other: &Self) -> bool {
        self.sequence_id.eq(&other.sequence_id)
    }
}

impl PartialOrd<Self> for Payload {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Payload {
    fn cmp(&self, other: &Self) -> Ordering {
        self.sequence_id.cmp(&other.sequence_id)
    }
}
