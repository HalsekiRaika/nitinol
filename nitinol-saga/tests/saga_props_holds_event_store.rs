#[path = "common/helpers.rs"]
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

/// Correlation rule of [`TrivialSaga`]: the single process instance every
/// `Triggered` belongs to.
const TRIVIAL_SAGA_ID: &str = "saga-props-direct";

#[derive(Default)]
struct TrivialSaga;

#[async_trait]
impl Saga for TrivialSaga {
    type SubscribedEvent = Triggered;
    type Event = Recorded;
    type ScheduledMessage = ();
    type Error = std::convert::Infallible;

    fn correlate(_event: &Self::SubscribedEvent) -> Option<SagaId> {
        Some(SagaId::new(TRIVIAL_SAGA_ID))
    }

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

    let saga_id = SagaId::new(TRIVIAL_SAGA_ID);

    let _proxy = SagaProps::<TrivialSaga>::new(saga_id, saga_store, TrivialSaga::default)
        .with_codec(system.codec::<Recorded>())
        .with_subscription(
            upstream_store,
            system.codec::<Triggered>(),
            SequenceCursor::Global { after: 0 },
        )
        .spawn(system.process_system())
        .await;
}
