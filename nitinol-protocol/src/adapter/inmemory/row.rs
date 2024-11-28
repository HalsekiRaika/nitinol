use std::cmp::Ordering;
use time::OffsetDateTime;

#[derive(Debug, Clone)]
pub struct Row {
    pub id: String,
    pub seq: i64,
    pub registry_key: String,
    pub bytes: Vec<u8>,
    pub created_at: OffsetDateTime
}

impl Eq for Row {}

impl PartialEq<Self> for Row {
    fn eq(&self, other: &Self) -> bool {
        self.seq.eq(&other.seq)
    }
}

impl PartialEq<i64> for Row {
    fn eq(&self, other: &i64) -> bool {
        self.seq.eq(other)
    }
}

impl PartialOrd<i64> for Row {
    fn partial_cmp(&self, other: &i64) -> Option<Ordering> {
        Some(self.seq.cmp(other))
    }
}

impl PartialOrd<Self> for Row {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Row {
    fn cmp(&self, other: &Self) -> Ordering {
        self.seq.cmp(&other.seq)
            .then_with(|| self.created_at.cmp(&other.created_at))
            .then_with(|| self.id.cmp(&other.id))
    }
}
