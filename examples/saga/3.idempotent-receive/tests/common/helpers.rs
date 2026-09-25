//! Wiring shared by the idempotent-receive example's integration tests.
//!
//! Each test binary compiles this module in full but uses only the subset it
//! needs, so per-binary dead code is expected.

#![allow(dead_code)]

use std::sync::Arc;
use std::time::Duration;

use futures_util::TryStreamExt;

use nitinol_eventsource::codec::Codec;
use nitinol_eventsource::system::{EventSourceSystem, StoreSet};
use nitinol_eventsource::{AggregateProxy, Event};
use nitinol_persistence::store::{EventStore, InMemoryEventStore};
use nitinol_persistence::{AggregateId, LoadQuery, LoadedEvent};
use nitinol_runtime::ProcessSystem;
use nitinol_saga::{SagaDefaultStoreExt, SagaManagerProxy};

use saga_idempotent_receive::order::Order;
use saga_idempotent_receive::saga::{ReservationRequested, ReservationSaga};
use saga_idempotent_receive::stock_item::{Reserve, StockItem, StockReserved};

/// The stock item every order in these tests reserves against.
pub const SKU: &str = "sku-cobalt-ingot";

/// How much one order reserves.
///
/// Anything other than zero works: the quantity is part of the stored payload
/// the fingerprint compares, so a second application folded into the first
/// record rather than appended beside it still shows up as a difference.
pub const ORDER_QUANTITY: u64 = 7;

/// Budget for an order to reach the stock item through the outbox.  The manager
/// polls its upstream every 250ms, so this is generous rather than tight — a
/// deadline, not a synchronisation device: every wait below polls an observable
/// condition.
pub const DISPATCH_TIMEOUT: Duration = Duration::from_secs(20);

/// Settle window used only in front of a negative assertion.
///
/// The saga appends its tell marker and *then* dispatches it, so the moment the
/// second decision becomes visible on the saga's stream the re-dispatched command
/// may still be in flight.  There is no earlier observable point, so the
/// re-dispatch is given a bounded window to land before the "nothing changed"
/// assertions run.  Mirrors the settle used by
/// `examples/saga/2.fanout-pattern/tests/redelivery_is_noop.rs`.
pub const REDISPATCH_SETTLE: Duration = Duration::from_millis(750);

/// The one store an incarnation is wired with.
///
/// `EventStore` is stream-keyed, so a single instance holds the order's stream,
/// the stock item's stream and the saga's own journal side by side; splitting
/// them across instances is a deployment choice, not something the pattern
/// needs.
pub fn single_store() -> Arc<dyn EventStore> {
    Arc::new(InMemoryEventStore::default())
}

/// A system bound to `store`, with no saga on it.
///
/// The direct-dispatch test drives the receiving aggregate itself, so it needs
/// the aggregates without the process manager that would also address them.
pub async fn spawn_system(store: Arc<dyn EventStore>) -> EventSourceSystem<StoreSet> {
    let ps = ProcessSystem::new().await;
    EventSourceSystem::builder(ps)
        .with_event_store(store)
        .build()
}

/// Everything one incarnation of the example owns.
///
/// `system` is kept because it is the handle to the `ProcessSystem` the
/// aggregates and the manager run on; dropping the last handle early would take
/// the incarnation down.
pub struct ReservationWorld {
    pub system: EventSourceSystem<StoreSet>,
    pub order: AggregateProxy<Order>,
    pub manager: SagaManagerProxy<ReservationSaga>,
}

/// Spawn a complete incarnation: a fresh `ProcessSystem`, the order aggregate,
/// and the saga manager subscribed to the order's stream.
///
/// The store is a parameter because the reprocessing test hands the same durable
/// state to two successive incarnations, which is what makes the receiving
/// aggregate restore its processed-id set from its own stream rather than inherit
/// it from memory.
pub async fn spawn_world(order_id: &AggregateId, store: Arc<dyn EventStore>) -> ReservationWorld {
    let system = spawn_system(store).await;

    let order = system.spawn_aggregate::<Order>(order_id.clone()).await;

    let manager = spawn_manager(&system, order_id).await;

    ReservationWorld {
        system,
        order,
        manager,
    }
}

/// Spawn (or re-spawn) the saga manager over an existing system.
///
/// The cursor starts at `after: 0` and lives in memory, so a manager spawned a
/// second time over the same store replays the order's fact event — the
/// at-least-once redelivery the pattern has to absorb.
pub async fn spawn_manager(
    system: &EventSourceSystem<StoreSet>,
    order_id: &AggregateId,
) -> SagaManagerProxy<ReservationSaga> {
    let system_for_producer = system.clone();
    let system_for_factory = system.clone();

    system
        .saga_manager_props(system.subscription(order_id), move || {
            ReservationSaga::new(system_for_producer.clone())
        })
        .with_crash_restart_factory(move |payload: &[u8]| {
            ReservationSaga::crash_restart_intent(&system_for_factory, payload)
        })
        .spawn(system.process_system())
        .await
}

// Store observation

pub async fn load_stream(store: &Arc<dyn EventStore>, key: &str) -> Vec<LoadedEvent> {
    store
        .load(LoadQuery::by_stream(key))
        .await
        .expect("load stream must succeed")
        .try_collect()
        .await
        .expect("collect stream must succeed")
}

/// Positional identity of every record in a stream: stream sequence, the
/// store-wide `global_sequence` assigned at commit, and the stored payload.
///
/// Comparing two fingerprints of the same stream taken before and after a
/// re-dispatch distinguishes "the first write was left intact" from "the record
/// was rewritten with equal content".
pub type StreamFingerprint = Vec<(u64, u64, Vec<u8>)>;

pub async fn fingerprint(store: &Arc<dyn EventStore>, key: &str) -> StreamFingerprint {
    load_stream(store, key)
        .await
        .into_iter()
        .map(|event| {
            (
                event.sequence,
                event.global_sequence,
                event.payload.to_vec(),
            )
        })
        .collect()
}

/// Read a stored record back through the codec its event type names.
///
/// The format belongs to the event, so the test reads it from there rather than
/// naming a serializer of its own: a second definition of the format would let
/// the two drift apart while both kept passing.
pub fn decode<E: Event>(event: &LoadedEvent) -> E {
    <E::Codec as Codec<E>>::decode(&event.payload)
        .expect("a record this example wrote must decode with the codec its event names")
}

/// The operation ids the stock item's own stream records, in stream order.
///
/// This is the observation that makes "the id is on the event" checkable: a
/// pattern that kept the processed ids only in the receiver's memory would leave
/// this list empty after a restart.
pub fn recorded_reservations(events: &[LoadedEvent]) -> Vec<String> {
    events
        .iter()
        .filter(|event| event.event_type == StockReserved::EVENT_TYPE)
        .map(|event| decode::<StockReserved>(event).reservation)
        .collect()
}

/// The operation ids the saga's own decisions name, in stream order.
///
/// One per time the saga processed an upstream event, which is what makes a
/// re-dispatch distinguishable from a suppressed one.
pub fn requested_reservations(events: &[LoadedEvent]) -> Vec<String> {
    events
        .iter()
        .filter(|event| event.event_type == ReservationRequested::EVENT_TYPE)
        .map(|event| decode::<ReservationRequested>(event).reservation)
        .collect()
}

/// Poll the stream `key` until it holds at least `expected` records.
pub async fn wait_for_records(
    store: &Arc<dyn EventStore>,
    key: &str,
    expected: usize,
    timeout: Duration,
) -> Vec<LoadedEvent> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let events = load_stream(store, key).await;
        if events.len() >= expected {
            return events;
        }
        if std::time::Instant::now() >= deadline {
            panic!(
                "timed out waiting for {expected} records on {key} (had {})",
                events.len()
            );
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// Poll the saga's own stream until it holds at least `expected` records of
/// [`ReservationRequested`] — one per decision the saga committed.
pub async fn wait_for_decisions(
    store: &Arc<dyn EventStore>,
    saga_key: &str,
    expected: usize,
    timeout: Duration,
) -> Vec<LoadedEvent> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let events = load_stream(store, saga_key).await;
        let decisions = events
            .iter()
            .filter(|event| event.event_type == ReservationRequested::EVENT_TYPE)
            .count();
        if decisions >= expected {
            return events;
        }
        if std::time::Instant::now() >= deadline {
            panic!(
                "timed out waiting for {expected} ReservationRequested records on {saga_key} \
                 (had {decisions} among {} records)",
                events.len()
            );
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

// Aggregate observation

/// A reference to the stock item that owns the stream `key`.
pub fn stock_ref(system: &EventSourceSystem<StoreSet>, key: &str) -> AggregateProxy<StockItem> {
    system.aggregate_proxy::<StockItem>(AggregateId::new(key))
}

/// Round-trip a command through the stock item's mailbox, so a re-dispatch that
/// was merely still queued cannot be mistaken for one that was absorbed.
///
/// `AggregateProxy::tell` — how the outbox dispatches — returns once the command
/// is queued, and `ask` through a reference to the same aggregate resolves to the
/// same activation, whose process loop is single-threaded and FIFO.  The answer
/// to this `ask` therefore cannot arrive before every command queued ahead of it
/// has been handled.
///
/// The command sent is a repeat of `reservation`, which the pattern answers with
/// an acceptance that appends nothing: draining the mailbox this way adds no
/// record for the assertions that follow to see.
pub async fn drain_stock_item(
    system: &EventSourceSystem<StoreSet>,
    stock_key: &str,
    reservation: &str,
) {
    stock_ref(system, stock_key)
        .ask(Reserve {
            reservation: reservation.to_owned(),
            stock: stock_key.to_owned(),
            quantity: ORDER_QUANTITY,
        })
        .await
        .expect("a repeat of an applied reservation must be accepted");
}
