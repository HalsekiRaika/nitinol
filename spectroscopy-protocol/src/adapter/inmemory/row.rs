use std::cmp::Ordering;

#[derive(Debug, Clone)]
pub struct Row {
    id: String,
    seq: i64,
    registry_key: String,
    bytes: Vec<u8>,
}

impl Eq for Row {}

impl PartialEq<Self> for Row {
    fn eq(&self, other: &Self) -> bool {
        self.id.eq(&other.id)
            || self.seq.eq(&other.seq)
    }
}

impl PartialOrd<Self> for Row {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Row {
    fn cmp(&self, other: &Self) -> Ordering {
        self.seq
            .cmp(&other.seq)
            .then_with(|| self.id.cmp(&other.id))
            .then_with(|| self.bytes.cmp(&other.bytes))
    }
}
