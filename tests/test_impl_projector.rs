use spectroscopy::identifier::EntityId;
use spectroscopy::protocol::errors::ProtocolError;
use spectroscopy::protocol::{Payload, ReadProtocol};
use std::cmp::Ordering;
use std::collections::BTreeSet;
use std::sync::Arc;
use tokio::sync::RwLock;

#[derive(Debug, Clone)]
pub struct Row {
    id: String,
    seq: i64,
    version: i64,
    registry_key: String,
    bytes: Vec<u8>,
}

impl Eq for Row {}

impl PartialEq<Self> for Row {
    fn eq(&self, other: &Self) -> bool {
        self.id.eq(&other.id)
            || self.seq.eq(&other.seq)
            || self.version.eq(&other.version)
            || self.registry_key.eq(&other.registry_key)
            || self.bytes.eq(&other.bytes)
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
            .then_with(|| self.version.cmp(&other.version))
            .then_with(|| self.registry_key.cmp(&other.registry_key))
            .then_with(|| self.bytes.cmp(&other.bytes))
    }
}

pub struct InMemoryEventStore {
    store: Arc<RwLock<BTreeSet<Row>>>,
}

impl Default for InMemoryEventStore {
    fn default() -> Self {
        Self {
            store: Arc::new(RwLock::new(BTreeSet::new())),
        }
    }
}

impl InMemoryEventStore {
    pub async fn write(
        &self,
        id: String,
        seq: i64,
        version: i64,
        registry_key: String,
        bytes: Vec<u8>,
    ) {
        let row = Row {
            id,
            seq,
            registry_key,
            version,
            bytes,
        };
        self.store.write().await.insert(row);
    }
}

#[async_trait::async_trait]
impl ReadProtocol for InMemoryEventStore {
    async fn read(
        &self,
        id: &EntityId,
        start: i64,
        to: i64,
    ) -> Result<BTreeSet<Payload>, ProtocolError> {
        println!("Reading from {} to {}", start, to);

        let col = self
            .store
            .read()
            .await
            .iter()
            .filter(|row| row.id.eq(&id.to_string()))
            .filter(|row| start <= row.seq && row.seq <= to)
            .cloned()
            .map(|row| Payload {
                sequence_id: row.seq,
                registry_key: row.registry_key,
                version: row.version,
                bytes: row.bytes,
            })
            .collect::<BTreeSet<Payload>>();
        Ok(col)
    }
}

#[test]
fn main() { /* Compile-Only */
}
