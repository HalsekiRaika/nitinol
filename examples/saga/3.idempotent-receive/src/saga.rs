//! The process manager that turns a placed order into one reservation.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use nitinol_eventsource::system::SystemHandle;
use nitinol_eventsource::{AggregateProxy, Event};
use nitinol_persistence::{AggregateId, EventType, Family, TypeName};
use nitinol_saga::{Saga, SagaContext, SagaEffect, SagaId, SagaSubscribe, TellIntent};

use crate::codec::JsonCodec;
use crate::order::OrderPlaced;
use crate::stock_item::{Reserve, StockItem};

/// The hosting system, as the saga is handed it.
///
/// A saga's producer is given the system it was spawned from rather than a
/// clone prepared outside the spawn, and what it is given has the store's type
/// erased — one type for every deployment, since a saga type is written before
/// anyone assembles a store. The journal survives the erasure, which is all an
/// aggregate reference needs.
pub type ReservationSystem = SystemHandle;

/// A reference to the stock item that owns the stream `key`.
///
/// Routing is "given a stream key, name the aggregate", which is what an
/// [`AggregateProxy`] already is — a reference resolved when a command is
/// dispatched rather than when the reference is built — so this example keeps
/// no registry of live items.  The reference is built without awaiting, which
/// is what lets the crash-restart factory below produce one at all.
fn stock_ref(system: &ReservationSystem, key: &str) -> AggregateProxy<StockItem> {
    system.aggregate_proxy::<StockItem>(AggregateId::new(key))
}

/// The saga's own decision event: "this order's reservation was asked for".
///
/// It restates the operation id, the target and the size rather than pointing
/// back at the trigger, so the saga's stream can be read on its own.
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
pub struct ReservationSaga {
    system: ReservationSystem,
}

impl ReservationSaga {
    pub fn new(system: ReservationSystem) -> Self {
        Self { system }
    }

    /// One reservation process per order.
    ///
    /// This is the single definition of that rule: [`Saga::correlate`] answers
    /// with it, and it names the stream the instance persists to.
    pub fn instance_id(order_id: &str) -> SagaId {
        SagaId::new(format!("{order_id}/reservation-saga"))
    }

    /// The operation id for the reservation `event` owes.
    ///
    /// It is a function of the upstream event's own domain content because a
    /// re-dispatched marker is only one of the ways the same operation is asked
    /// for twice — the others reach [`Saga::handle`] again and build the command
    /// from scratch (see [`SagaEffect::tell`]): a batch that never landed leaves
    /// its input unconsumed, and two activations of one instance can both decide
    /// from an input before either's batch is durable.  An id minted per `handle`
    /// call survives the marker route and fails those, invisibly, until the stock
    /// item applies the reservation twice.  `order_id` is the derivation base
    /// because an order owes its reservation exactly once: something every order
    /// shares would be just as stable and would make the *next* order's
    /// reservation look like a repeat the receiver then silently withholds.
    pub fn reservation_id(event: &OrderPlaced) -> String {
        format!("{}/reservation", event.order_id)
    }

    /// Rebuild a pending reservation from the bytes a `TellRequested` marker
    /// kept for it.
    ///
    /// After a full process crash only the marker survives, so the command has
    /// to be rebuilt from its serialized form — which is why [`Reserve`] carries
    /// the target's stream key as well as the operation id.  That what comes
    /// back is the command and target originally dispatched is this function's
    /// obligation, not the framework's; when and how the bytes are handed over
    /// is stated on [`SagaEffect::tell`].
    pub fn crash_restart_intent(
        system: &ReservationSystem,
        crash_restart_payload: &[u8],
    ) -> Option<TellIntent> {
        let cmd: Reserve = serde_json::from_slice(crash_restart_payload).ok()?;
        let target = stock_ref(system, &cmd.stock);
        Some(TellIntent::new::<StockItem, Reserve, _>(target, cmd))
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

    // The saga keeps no state, so `apply` is left to its no-op default.
    //
    // Whether the reservation was applied is already recorded — the stock item's
    // own stream says so.  Mirroring it here would give the same fact two owners
    // that a crash between the two writes could leave disagreeing.

    /// Nothing here suppresses a repeat, on purpose.
    ///
    /// Two things could: the framework drops an input whose decision is already
    /// durable before this method sees it, and the stock item answers a
    /// reservation it has already applied with a write-free acceptance.  Both are
    /// backed by a stream.  A check inside this method would not be: it would hold
    /// only as long as this incarnation's memory does, and it would do nothing at
    /// all about the dispatch a pending marker repeats, which never passes through
    /// here.  The operation id is what lets the stock item recognise that repeat.
    async fn handle(
        &mut self,
        input: Self::Subscribe,
        _ctx: &mut SagaContext,
    ) -> Result<SagaEffect<Self::Event>, Self::Error> {
        let Observed::OrderPlaced(event) = input;
        let reservation = Self::reservation_id(&event);
        let stock = StockItem::stream_key(&event.sku);

        let decision = SagaEffect::persist(ReservationRequested {
            reservation: reservation.clone(),
            stock: stock.clone(),
            quantity: event.quantity,
        });

        // `combine` folds adjacent `Persist` branches, so the decision and its
        // outbox marker reach the store as one append.  A crash between the two
        // could otherwise leave the saga believing it had asked for a
        // reservation it never enqueued.
        let target = stock_ref(&self.system, &stock);
        Ok(decision.combine(SagaEffect::tell::<StockItem, Reserve, _>(
            target,
            Reserve {
                reservation,
                stock,
                quantity: event.quantity,
            },
        )))
    }
}
