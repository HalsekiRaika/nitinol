//! The process manager: it observes the order, decides a reservation, and says
//! what that decision means for the inventory.
//!
//! A saga's own work is only the deciding.  It persists its decision on its own
//! stream and nothing else — there is no way to express "send this command" in
//! a [`SagaEffect`].  What a decision implies for everybody else is the pure
//! mapping [`Saga::dispatch`] answers, and carrying what that mapping names to
//! the aggregates it addresses is a *delivery host*'s work (wired in
//! `src/main.rs`).

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use nitinol::eventsource::Event;
use nitinol_persistence::AggregateId;
use nitinol_saga::{Saga, SagaContext, SagaDelivery, SagaEffect, SagaId, SagaSubscribe};

use crate::codec::JsonCodec;
use crate::inventory::{Inventory, Reserve};
use crate::order::OrderPlaced;

/// The saga's own decision: "a reservation has been decided for this sku".
///
/// It is a fact on the saga's own stream, and it is the whole of what a
/// delivery host gets to read — so everything the delivery needs has to be on
/// it:
///
/// * `inventory` — the stream key of the aggregate the decision is addressed
///   to.  The saga was told which inventory to use at spawn time and writes
///   that address down here, rather than holding a live reference the host
///   would have no way to recover.
/// * `reservation` — the operation id the receiver recognises a repeat by.
///   Delivery is at-least-once, so the same decision can be carried twice; the
///   id is the domain's own, because the framework attaches no envelope of its
///   own to a delivered command.
/// * `sku` — what the reservation is for, which is the domain payload.
#[derive(Clone, Debug, Serialize, Deserialize, Event)]
#[event(family = "saga.example", codec = JsonCodec)]
pub struct ReservationRequested {
    pub inventory: String,
    pub sku: String,
    pub reservation: String,
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

/// Turns a placed order into a reservation decision addressed to `inventory`.
pub struct ReservationSaga {
    /// Which inventory this process reserves against.  An address, not a live
    /// proxy: it is what the decision below carries, and a host reads the
    /// decision long after the incarnation that wrote it is gone.
    pub inventory: AggregateId,
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
    // reservation it decided is already on its own stream and the inventory's,
    // and mirroring it here would give one fact a third owner.

    /// What one decision means for everybody else.
    ///
    /// A pure associated function of the decision alone — no `&self`, because a
    /// delivery host keeps no saga resident: it reads decisions out of the
    /// store, including ones persisted by an incarnation that no longer exists,
    /// so there is nothing for it to ask.  Everything it needs is therefore on
    /// the event, which is why [`ReservationRequested`] carries the address and
    /// the operation id.
    ///
    /// Returning an empty vector is legitimate and is what the trait's default
    /// does: a decision with no consequence outside the saga owes no delivery.
    fn dispatch(event: &Self::Event) -> Vec<SagaDelivery> {
        vec![SagaDelivery::new::<Inventory, Reserve>(
            AggregateId::new(event.inventory.clone()),
            Reserve {
                sku: event.sku.clone(),
                reservation: event.reservation.clone(),
            },
        )]
    }

    async fn handle(
        &mut self,
        input: Self::Subscribe,
        ctx: &mut SagaContext,
    ) -> Result<SagaEffect<Self::Event>, Self::Error> {
        // The alphabet has one variant, so this binding is exhaustive — and it
        // is what a second variant would make the compiler reject.
        let Observed::OrderPlaced(event) = input;

        // The operation id is derived from the input's own position, so it is
        // the same id whenever this record is decided again — which is what
        // makes the inventory's recognition of a repeat work across a
        // redelivery of the upstream event as well as of the command.
        let reservation = format!(
            "{}#{}",
            ctx.upstream_aggregate_id().as_str(),
            ctx.upstream_sequence()
        );

        Ok(SagaEffect::persist(ReservationRequested {
            inventory: self.inventory.as_str().to_owned(),
            sku: event.sku,
            reservation,
        }))
    }
}
