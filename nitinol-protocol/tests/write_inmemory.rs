use nitinol_core::errors::{DeserializeError, SerializeError};
use nitinol_core::event::Event;
use nitinol_protocol::adapter::inmemory::InMemoryEventStore;
use nitinol_protocol::io::{ReadProtocol, WriteProtocol};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, Serialize)]
pub enum MyEvent {
    Added,
    Removed
}

impl Event for MyEvent {
    const REGISTRY_KEY: &'static str = "my-event";
    
    fn as_bytes(&self) -> Result<Vec<u8>, SerializeError> {
        Ok(serde_json::to_vec(self)?)
    }
    
    fn from_bytes(bytes: &[u8]) -> Result<Self, DeserializeError> {
        Ok(serde_json::from_slice(bytes)?)
    }
}

#[tokio::test]
async fn write() {
    let inmemory = InMemoryEventStore::default();
    let writer = WriteProtocol::new(inmemory.clone());
    let reader = ReadProtocol::new(inmemory);
    
    writer.write("aggregate_1", &MyEvent::Added, 1).await.unwrap();
    writer.write("aggregate_1", &MyEvent::Added, 2).await.unwrap();
    writer.write("aggregate_1", &MyEvent::Added, 3).await.unwrap();
    writer.write("aggregate_1", &MyEvent::Added, 4).await.unwrap();
    writer.write("aggregate_1", &MyEvent::Added, 5).await.unwrap();
    writer.write("aggregate_1", &MyEvent::Added, 6).await.unwrap();
    writer.write("aggregate_1", &MyEvent::Added, 7).await.unwrap();
    writer.write("aggregate_1", &MyEvent::Added, 8).await.unwrap();
    writer.write("aggregate_1", &MyEvent::Added, 9).await.unwrap();
    
    let read = reader.read_to_latest("aggregate_1", 0).await.unwrap();
    println!("{:?}", read);
    assert_eq!(read.len(), 9);
}