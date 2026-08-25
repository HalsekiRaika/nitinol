use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use nitinol::eventsource::Event;
use nitinol_eventsource::AggregateProxy;
use nitinol_saga::{Saga, SagaContext, SagaEffect, SagaId};

use crate::inventory::{Inventory, Reserve};
use crate::order::OrderPlaced;

#[derive(Clone, Debug, Serialize, Deserialize, Event)]
#[event(family = "saga.example")]
pub struct ReservationRequested {
    pub sku: String,
}

pub struct ReservationSaga {
    pub inventory: AggregateProxy<Inventory>,
}

impl ReservationSaga {
    /// This example runs one reservation process for every order, so its
    /// correlation rule is the single definition point for that instance's
    /// identity: `Saga::correlate` answers with it, and the spawn site names the
    /// instance with it.
    pub fn instance_id() -> SagaId {
        SagaId::new("example-reservation-saga")
    }
}

#[async_trait]
impl Saga for ReservationSaga {
    type SubscribedEvent = OrderPlaced;
    type Event = ReservationRequested;
    type ScheduledMessage = ();
    type Error = std::convert::Infallible;

    fn correlate(_event: &Self::SubscribedEvent) -> Option<SagaId> {
        Some(Self::instance_id())
    }

    fn apply(&mut self, _event: Self::Event) {}

    async fn handle(
        &mut self,
        event: Self::SubscribedEvent,
        _ctx: &mut SagaContext,
    ) -> Result<SagaEffect<Self::Event>, Self::Error> {
        let persist = SagaEffect::persist(ReservationRequested {
            sku: event.sku.clone(),
        });
        let tell = SagaEffect::tell(self.inventory.clone(), Reserve { sku: event.sku });
        Ok(persist.combine(tell))
    }
}
