#[path = "common/helpers.rs"]
mod common;
use common::JsonCodec;

use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use nitinol_eventsource::{system::EventSourceSystem, Event};
use nitinol_persistence::provider::Persistence;
use nitinol_persistence::store::{EventStore, InMemoryEventStore};
use nitinol_persistence::{EventType, Family, TypeName};
use nitinol_runtime::ProcessSystem;
use nitinol_saga::{Saga, SagaContext, SagaEffect, SagaId, SagaProps, SagaSubscribe, Subscription};

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Triggered;

impl Event for Triggered {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("saga.direct"), TypeName::new("Triggered"));
    type Codec = JsonCodec;
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Recorded;

impl Event for Recorded {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("saga.direct"), TypeName::new("Recorded"));
    type Codec = JsonCodec;
}

/// Correlation rule of [`TrivialSaga`]: the single process instance every
/// `Triggered` belongs to.
const TRIVIAL_SAGA_ID: &str = "saga-props-direct";

/// The whole alphabet this saga reacts to: one observed source, no deadline.
#[derive(SagaSubscribe)]
enum Observed {
    Triggered(Triggered),
}

#[derive(Default)]
struct TrivialSaga;

#[async_trait]
impl Saga for TrivialSaga {
    type Subscribe = Observed;
    type Event = Recorded;
    type Error = std::convert::Infallible;

    fn correlate(_input: &Self::Subscribe) -> Option<SagaId> {
        Some(SagaId::new(TRIVIAL_SAGA_ID))
    }

    async fn handle(
        &mut self,
        _input: Self::Subscribe,
        _ctx: &mut SagaContext,
    ) -> Result<SagaEffect<Self::Event>, Self::Error> {
        Ok(SagaEffect::None)
    }
}

#[tokio::test]
async fn saga_props_spawns_with_arc_dyn_event_store() {
    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::builder(ps)
        .with_persistence(Persistence::in_memory())
        .build();
    let saga_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let upstream_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());

    let saga_id = SagaId::new(TRIVIAL_SAGA_ID);

    let _proxy = SagaProps::<TrivialSaga>::new(saga_id, saga_store, |_| TrivialSaga)
        .subscribed_to::<Triggered>(Subscription::global(&upstream_store))
        .spawn(system.process_system())
        .await;
}
