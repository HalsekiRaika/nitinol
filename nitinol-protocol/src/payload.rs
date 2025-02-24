use std::cmp::Ordering;
use std::fmt::{Debug, Formatter};
use time::OffsetDateTime;
use nitinol_core::errors::{DeserializeError, SerializeError};
use nitinol_core::event::Event;
use nitinol_core::identifier::EntityId;

/// Basic format of the data to be saved.
#[derive(Clone)]
#[cfg_attr(feature = "sqlx", derive(sqlx::FromRow))]
pub struct Payload {
    /// Aggregate entity identifier
    pub id: String,
    /// Unique sequence value at a specific Entity
    pub sequence_id: i64,
    /// Unique id for each data format used in [`ResolveMapping`](nitinol_core::mapping::ResolveMapping)
    pub registry_key: String,
    /// Data body in binary format
    pub bytes: Vec<u8>,
    /// Time the Event was generated
    pub created_at: OffsetDateTime
}

impl Payload {
    pub fn new<E: Event>(aggregate_id: EntityId, seq: i64, event: &E) -> Result<Self, SerializeError> {
        Ok(Self {
            id: aggregate_id.to_string(),
            sequence_id: seq,
            registry_key: E::REGISTRY_KEY.to_string(),
            bytes: event.as_bytes()?,
            created_at: OffsetDateTime::now_utc()
        })
    }
    
    pub fn to_event<E: Event>(&self) -> Result<E, DeserializeError> {
        E::from_bytes(&self.bytes)
    }
}

impl Debug for Payload {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct(format!("Payload#{}", self.registry_key).as_str())
            .field("id", &self.id)
            .field("sequence", &self.sequence_id)
            .field("bytes", &format!("<{} bytes>", self.bytes.len()))
            .field("created_at", &self.created_at)
            .finish()
    }
}

impl Eq for Payload {}

impl PartialEq<Self> for Payload {
    fn eq(&self, other: &Self) -> bool {
        self.sequence_id.eq(&other.sequence_id)
        && self.id.eq(&other.id)
        && self.created_at.eq(&other.created_at)
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
            .then_with(|| self.created_at.cmp(&other.created_at))
            .then_with(|| self.id.cmp(&other.id))
    }
}
