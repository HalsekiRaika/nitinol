use std::cmp::Ordering;


pub mod errors;
mod read;

pub use self::read::*;

#[derive(Debug, Clone)]
pub struct Payload {
    pub sequence_id: i64,
    pub registry_key: String,
    pub bytes: Vec<u8>
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

