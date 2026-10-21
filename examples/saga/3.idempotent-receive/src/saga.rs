//! The process manager that turns a placed order into one reservation.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use nitinol_eventsource::Event;
use nitinol_persistence::{AggregateId, EventType, Family, TypeName};
use nitinol_saga::{Saga, SagaContext, SagaDelivery, SagaEffect, SagaId, SagaSubscribe};

use crate::codec::JsonCodec;
use crate::order::OrderPlaced;
use crate::stock_item::{Reserve, StockItem};

/// The saga's own decision event: "this order's reservation was asked for".
///
/// Every field is here because [`Saga::dispatch`] needs it and cannot reach
/// anywhere else for it.  That mapping is handed this event and nothing else —
/// not the saga instance, not the trigger it was decided from, not the stream
/// the record was read off — so a decision that left something out could not be
/// turned into a command at all:
///
/// * `reservation` is the operation id the dispatched command carries, and the
///   only thing by which the receiver can recognise a second delivery of this
///   operation.  An id minted inside the delivery path instead would be a fresh
///   one every time, which is the mistake this crate exists to rule out.
/// * `stock` is the stream key of the aggregate the command is addressed to.
///   Nothing downstream knows the order's `sku`, so whom to ask is a fact of
///   the decision too.
/// * `quantity` is what the command asks for.
///
/// It restates all three rather than pointing back at the trigger, so the
/// saga's own stream can be read on its own — by a delivery host now, by an
/// archive replay later.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReservationRequested {
    /// The operation id the dispatched command carries.
    pub reservation: String,
    /// Stream key of the stock item it is addressed to.
    pub stock: String,
    /// How much it reserves.
    pub quantity: u64,
}

impl Event for ReservationRequested {
    const EVENT_TYPE: EventType = EventType::new(
        Family::new("saga.idempotent"),
        TypeName::new("ReservationRequested"),
    );

    type Codec = JsonCodec;
}

/// The whole alphabet this saga reacts to: the order's fact event, and no
/// deadline of its own.
///
/// Everything the saga reacts to is declared here, and everything reaches it
/// through the one [`Saga::handle`] entry point.  A saga that later had to
/// notice the stock item's reply, or give the reservation a deadline to arrive
/// within, would gain a variant here — and the `match`es that answer the
/// alphabet would stop compiling until it was handled.
#[derive(SagaSubscribe)]
pub enum Observed {
    OrderPlaced(OrderPlaced),
}

/// Asks the stock item to reserve what a placed order owes it.
///
/// A unit struct, and it has to be nothing more: a saga has no way to express
/// "send this command" — [`SagaEffect`] persists the saga's own decisions and
/// sets the saga's own deadlines, and that is all — so there is no reference to
/// the stock item for this type to hold.  Whom the decision reaches is
/// [`Saga::dispatch`]'s answer, and carrying it is a delivery host's work.
///
/// It keeps no state either, so [`Saga::apply`] is left to its no-op default.
/// Whether the reservation was applied is already recorded — the stock item's
/// own stream says so — and mirroring it here would give one fact two owners
/// that a crash between the two writes could leave disagreeing.
pub struct ReservationSaga;

impl ReservationSaga {
    /// One reservation process per order.
    ///
    /// This is the single definition of that rule: [`Saga::correlate`] answers
    /// with it, and it names the stream the instance persists to.
    pub fn instance_id(order_id: &str) -> SagaId {
        SagaId::new(format!("{order_id}/reservation-saga"))
    }

    /// The operation id for the reservation `event` owes.
    ///
    /// It is a function of the upstream event's own domain content because
    /// deciding is not something that happens exactly once either.  A decision
    /// can be durable while the position of the input it was decided from is
    /// not, and two activations of one instance can both decide from an input
    /// before either's append lands; in both cases the order's fact event
    /// reaches [`Saga::handle`] again and the command is built from scratch.  An
    /// id minted per `handle` call would make those two decisions two
    /// operations, and the stock item — which has nothing but the id to go on —
    /// would reserve twice.  Derived from the event, they are one operation
    /// asked for twice, which is exactly what the receiver absorbs.
    ///
    /// `order_id` is the derivation base because an order owes its reservation
    /// exactly once: something every order shares would be just as stable and
    /// would make the *next* order's reservation look like a repeat the receiver
    /// then silently withholds.
    pub fn reservation_id(event: &OrderPlaced) -> String {
        format!("{}/reservation", event.order_id)
    }
}

#[async_trait]
impl Saga for ReservationSaga {
    type Subscribe = Observed;
    type Event = ReservationRequested;
    type Error = std::convert::Infallible;

    fn correlate(input: &Self::Subscribe) -> Option<SagaId> {
        let Observed::OrderPlaced(event) = input;
        Some(Self::instance_id(&event.order_id))
    }

    /// What a reservation request means for everybody else: ask that stock item
    /// to reserve, under that operation id.
    ///
    /// An associated function taking the decision alone, because a delivery host
    /// keeps no saga resident — it reads decisions out of the store, including
    /// ones written by an incarnation that no longer exists — so there is no
    /// `&self` for it to ask and no trigger for it to reach back to.  That is
    /// what makes the operation id's presence on
    /// [`ReservationRequested`] structural rather than advisory: nothing here
    /// could mint one, and nothing could look one up.
    ///
    /// How often what this answers is carried out, and why that number is not
    /// one, is stated on
    /// [`SagaDeliveryProps`](nitinol_saga::SagaDeliveryProps).
    fn dispatch(event: &Self::Event) -> Vec<SagaDelivery> {
        vec![SagaDelivery::new::<StockItem, Reserve>(
            AggregateId::new(event.stock.clone()),
            Reserve {
                reservation: event.reservation.clone(),
                quantity: event.quantity,
            },
        )]
    }

    /// Nothing here suppresses a repeat, on purpose.
    ///
    /// Two things do, and both are backed by a stream: the framework drops an
    /// input whose position is already on this saga's own stream before this
    /// method sees it, and the stock item answers a reservation it has already
    /// applied with a write-free acceptance.  A check inside this method would
    /// be backed by neither — it would hold only as long as this incarnation's
    /// memory does, and it would do nothing at all about a *delivery* repeated
    /// from a decision already written, which never passes through here.  The
    /// operation id is what lets the stock item recognise that one.
    ///
    /// The decision is the whole of the effect.  There is no second branch to
    /// combine it with and no append to keep it atomic with: what the decision
    /// implies for the stock item is derived from this very record, later, by
    /// [`Saga::dispatch`], so there is nothing a crash could leave half-written.
    async fn handle(
        &mut self,
        input: Self::Subscribe,
        _ctx: &mut SagaContext,
    ) -> Result<SagaEffect<Self::Event>, Self::Error> {
        let Observed::OrderPlaced(event) = input;

        Ok(SagaEffect::persist(ReservationRequested {
            reservation: Self::reservation_id(&event),
            stock: StockItem::stream_key(&event.sku),
            quantity: event.quantity,
        }))
    }
}
