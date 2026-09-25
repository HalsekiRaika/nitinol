//! The process manager that turns a placed order into one reservation.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use nitinol_eventsource::system::{EventSourceSystem, StoreSet};
use nitinol_eventsource::{AggregateProxy, Event};
use nitinol_persistence::{AggregateId, EventType, Family, TypeName};
use nitinol_saga::{Saga, SagaContext, SagaEffect, SagaId, TellIntent};

use crate::codec::JsonCodec;
use crate::order::OrderPlaced;
use crate::stock_item::{Reserve, StockItem};

/// The system every aggregate in this example lives on.
type ReservationSystem = EventSourceSystem<StoreSet>;

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
    /// # Why this is a function of the event and nothing else
    ///
    /// Re-dispatching a pending `TellRequested` re-decodes bytes that were
    /// already persisted, so the command — and the id inside it — comes back
    /// identical without the domain doing anything.  That covers only one of the
    /// two ways the same operation is asked for twice.  The other is the *same
    /// upstream event reaching [`Saga::handle`] again* — a second manager
    /// incarnation catching up from the start of the order's stream, or another
    /// node processing it concurrently — which builds a brand new
    /// `TellRequested` with a freshly constructed command inside it.  An id
    /// minted per `handle` call (a fresh UUID, a clock reading) survives the
    /// first and fails the second, and the failure is invisible until the stock
    /// item applies the reservation a second time.
    ///
    /// So the id is derived from the upstream event's own domain content.
    /// `order_id` is the right derivation base here because an order owes its
    /// reservation exactly once: deriving from something every order shares
    /// would be equally stable and would make the *next* order's reservation
    /// look like a repeat, which the receiver would then silently withhold.
    ///
    /// The machine's coordinates are deliberately unavailable for this: a saga
    /// is handed the decoded event and nothing else — not the sequence the
    /// record was read at, not its `occurred_at` — because those are not facts
    /// the domain observes, and the same upstream fact re-read from a different
    /// position must still yield the same id.
    pub fn reservation_id(event: &OrderPlaced) -> String {
        format!("{}/reservation", event.order_id)
    }

    /// Rebuild a pending reservation from the bytes a `TellRequested` marker
    /// kept for it.
    ///
    /// After a full process crash the in-memory intent is gone and only the
    /// marker survives, so the command has to be reconstructed from its
    /// serialized form — which is why [`Reserve`] carries the target's stream
    /// key as well as the operation id.
    ///
    /// The bytes are handed over verbatim, so the operation id inside them comes
    /// back exactly as it was sent.  That the *command and target* this rebuilds
    /// are the ones originally dispatched is this function's obligation rather
    /// than the framework's: it is the same obligation across a deployment that
    /// changed this factory or the command's serialized shape.
    ///
    /// `None` is the framework's signal that this marker cannot be
    /// reconstructed; replay then records a `TellFailed` for it instead of
    /// dropping it unnoticed.
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
    type SubscribedEvent = OrderPlaced;
    type Event = ReservationRequested;
    type ScheduledMessage = ();
    type Error = std::convert::Infallible;

    fn correlate(event: &Self::SubscribedEvent) -> Option<SagaId> {
        Some(Self::instance_id(&event.order_id))
    }

    /// The saga keeps no state.
    ///
    /// Whether the reservation was applied is already recorded — the stock
    /// item's own stream says so.  Mirroring it here would give the same fact
    /// two owners that a crash between the two writes could leave disagreeing.
    fn apply(&mut self, _event: Self::Event) {}

    /// A redelivered fact event produces the same decision again, on purpose.
    ///
    /// Suppressing the second dispatch here would put the idempotence in the
    /// saga, where it holds only as long as this incarnation's memory does.
    /// Leaving it in the stock item puts it behind a durable stream instead, so
    /// it survives the restart it exists to protect against — and the operation
    /// id is what lets the stock item do so.
    async fn handle(
        &mut self,
        event: Self::SubscribedEvent,
        _ctx: &mut SagaContext,
    ) -> Result<SagaEffect<Self::Event>, Self::Error> {
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
