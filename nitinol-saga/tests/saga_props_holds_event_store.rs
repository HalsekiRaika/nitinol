mod common;
use common::JsonCodec;

use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use nitinol_eventsource::{system::EventSourceSystem, Event, SequenceCursor};
use nitinol_persistence::store::{EventStore, InMemoryEventStore};
use nitinol_persistence::{EventType, Family, TypeName};
use nitinol_runtime::ProcessSystem;
use nitinol_saga::{Saga, SagaContext, SagaEffect, SagaId, SagaProps};

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Triggered;

impl Event for Triggered {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("saga.direct"), TypeName::new("Triggered"));
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Recorded;

impl Event for Recorded {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("saga.direct"), TypeName::new("Recorded"));
}

#[derive(Default)]
struct TrivialSaga;

#[async_trait]
impl Saga for TrivialSaga {
    type SubscribedEvent = Triggered;
    type Event = Recorded;
    type State = ();
    type ScheduledMessage = ();
    type Error = std::convert::Infallible;

    fn apply(&mut self, _event: Self::Event) {}

    async fn handle(
        &mut self,
        _event: Self::SubscribedEvent,
        _ctx: &mut SagaContext,
    ) -> Result<SagaEffect<Self::Event>, Self::Error> {
        Ok(SagaEffect::None)
    }
}

#[tokio::test]
async fn saga_props_spawns_with_arc_dyn_event_store() {
    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::new(ps).with_codec::<JsonCodec>().build();
    let saga_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let upstream_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());

    let saga_id = SagaId::new("saga-props-direct");
    let routed = saga_id.clone();
    let route_fn = move |_event: &Triggered| -> Option<SagaId> { Some(routed.clone()) };

    let _proxy = SagaProps::<TrivialSaga>::new(saga_id, saga_store, TrivialSaga::default)
        .with_codec(system.codec::<Recorded>())
        .with_subscription(
            upstream_store,
            system.codec::<Triggered>(),
            SequenceCursor::Global { after: 0 },
            route_fn,
        )
        .spawn(system.process_system())
        .await;
}
