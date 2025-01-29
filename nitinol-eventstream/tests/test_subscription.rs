use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use nitinol::macros::Event;
use nitinol_core::identifier::ToEntityId;
use nitinol_eventstream::eventstream::EventStream;
use nitinol_eventstream::resolver::{DecodeMapping, SubscriptionMapper};
use nitinol_eventstream::subscriber::EventSubscriber;

#[derive(Debug, Clone, Event, Deserialize, Serialize)]
#[persist(enc = "serde_json::to_vec", dec = "serde_json::from_slice")]
pub enum DomainEvent {
    Signal1,
    Signal2,
    Signal3,
}

#[tokio::test]
async fn test_activate_eventstream() {
    let _stream = EventStream::default();
}

pub struct TestSubscriber;

impl SubscriptionMapper for TestSubscriber {
    fn mapping(mapping: &mut DecodeMapping<Self>) {
        mapping.register::<DomainEvent>();
    }
}

#[async_trait]
impl EventSubscriber<DomainEvent> for TestSubscriber {
    type Error = ();

    #[tracing::instrument(skip_all)]
    async fn on(&mut self, event: DomainEvent) -> Result<(), Self::Error> {
        tracing::debug!(name: "test-subscriber", "{:?}", event);
        Ok(())
    }
}

#[tokio::test]
async fn test_subscribe() {
    std::env::set_var("RUST_LOG", "trace");
    tracing_subscriber::registry().with(tracing_subscriber::fmt::layer()).init();

    let stream = EventStream::default();
    stream.subscribe(TestSubscriber).await;
    stream.publish("publisher_1".to_entity_id(), 1, &DomainEvent::Signal2).await;
    stream.publish("publisher_1".to_entity_id(), 2, &DomainEvent::Signal1).await;
    stream.publish("publisher_1".to_entity_id(), 3, &DomainEvent::Signal2).await;
    stream.publish("publisher_1".to_entity_id(), 4, &DomainEvent::Signal1).await;
    stream.publish("publisher_1".to_entity_id(), 5, &DomainEvent::Signal3).await;

    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
}
