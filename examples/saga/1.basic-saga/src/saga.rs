use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use nitinol::eventsource::Event;
use nitinol_eventsource::AggregateProxy;
use nitinol_saga::{Saga, SagaContext, SagaEffect, SagaId, SagaSubscribe};

use crate::codec::JsonCodec;
use crate::inventory::{Inventory, Reserve};
use crate::order::OrderPlaced;

#[derive(Clone, Debug, Serialize, Deserialize, Event)]
#[event(family = "saga.example", codec = JsonCodec)]
pub struct ReservationRequested {
    pub sku: String,
}

/// The whole alphabet this saga reacts to: one observed source, and no deadline
/// of its own.
///
/// Everything a saga reacts to is declared here, in one enum, and everything
/// reaches it through the one [`Saga::handle`] entry point.  Declaring it this
/// way is what makes a forgotten reaction a compile error rather than a silent
/// gap: adding a variant leaves the `match` in `handle` non-exhaustive, and the
/// spawn site owes the new source a subscription before `spawn` exists at all.
#[derive(SagaSubscribe)]
pub enum Observed {
    OrderPlaced(OrderPlaced),
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
    type Subscribe = Observed;
    type Event = ReservationRequested;
    type Error = std::convert::Infallible;

    fn correlate(_input: &Self::Subscribe) -> Option<SagaId> {
        Some(Self::instance_id())
    }

    // `apply` is left to its default: this saga keeps no state, because the
    // reservation it asked for is already recorded on the inventory's own
    // stream and mirroring it here would give one fact two owners.

    async fn handle(
        &mut self,
        input: Self::Subscribe,
        _ctx: &mut SagaContext,
    ) -> Result<SagaEffect<Self::Event>, Self::Error> {
        let Observed::OrderPlaced(event) = input;
        let persist = SagaEffect::persist(ReservationRequested {
            sku: event.sku.clone(),
        });
        let tell = SagaEffect::tell(self.inventory.clone(), Reserve { sku: event.sku });
        Ok(persist.combine(tell))
    }
}
