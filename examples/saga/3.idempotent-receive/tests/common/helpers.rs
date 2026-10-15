//! Wiring shared by the idempotent-receive example's integration tests.
//!
//! Each test binary compiles this module in full but uses only the subset it
//! needs, so per-binary dead code is expected.

#![allow(dead_code)]

use std::time::Duration;

use async_trait::async_trait;
use futures_util::TryStreamExt;

use nitinol_eventsource::codec::Codec;
use nitinol_eventsource::system::EventSourceSystem;
use nitinol_eventsource::{AggregateProxy, Event};
use nitinol_persistence::error::OffsetError;
use nitinol_persistence::provider::{CustomStore, PersistenceProvider, Unprovided};
use nitinol_persistence::store::{
    InMemoryEventStore, InMemoryOffsetStore, Journal, OffsetScope, OffsetStore,
};
use nitinol_persistence::{AggregateId, LoadQuery, LoadedEvent};
use nitinol_runtime::ProcessSystem;
use nitinol_saga::{
    SagaDeliveryId, SagaDeliveryProps, SagaDeliveryProxy, SagaManagerProps, SagaManagerProxy,
    Subscription,
};

use saga_idempotent_receive::order::{Order, OrderPlaced};
use saga_idempotent_receive::saga::{ReservationRequested, ReservationSaga};
use saga_idempotent_receive::stock_item::{StockItem, StockReserved};

/// The system an incarnation of this example assembles: one in-memory journal
/// and no other slot.
///
/// A store holding the journal alone *is* the journal, so the system is named
/// over a [`Journal`] rather than over an assembly wrapping one.
///
/// Where a delivery host keeps its progress is deliberately **not** on this
/// store.  The restart clauses below hand two successive hosts two different
/// offset stores over one journal, which a slot on the system would make
/// impossible without a second system — and a second system would also give the
/// stock item a second activation, mixing "the receiver recognised the repeat"
/// with "the receiver replayed first".  [`ExampleDeliveryStore`] is where the
/// host's two slots are named instead.
///
/// That is also why the manager here is started through
/// [`SagaManagerProps`] rather than through `host`: hosting a saga on a system
/// starts the delivery host too, over the system's own saga-offset slot, and a
/// slot is exactly what this one must not have.
pub type ExampleSystem = EventSourceSystem<Journal>;

/// The stock item every order in these tests reserves against.
pub const SKU: &str = "sku-cobalt-ingot";

/// How much one order reserves.
///
/// Anything other than zero works: the quantity is part of the stored payload
/// the fingerprint compares, so a second application folded into the first
/// record rather than appended beside it still shows up as a difference.
pub const ORDER_QUANTITY: u64 = 7;

/// Budget for an order to reach the stock item through the delivery host.  Two
/// poll loops stand in the way — the saga manager's subscription and the host's
/// — so this is generous rather than tight: a deadline, not a synchronisation
/// device, since every wait below polls an observable condition.
pub const DELIVERY_TIMEOUT: Duration = Duration::from_secs(20);

/// How often a delivery host under test asks the journal what is new.
///
/// Named so that the waits above are not tuned against a hidden default.
pub const POLL_INTERVAL: Duration = Duration::from_millis(25);

/// Number of records the saga's own stream holds for one decision: the decision
/// event and the position of the fact event it was derived from, committed by
/// the one append.
///
/// Nothing a delivery host does adds to this stream.  How far a host has got
/// lives in its offset — [`recorded_progress`] is how a test reads it — so
/// "the deliveries were made" and "the saga decided" are two separate
/// observations here rather than two record counts on one stream.
pub const DECISION_BLOCK_LEN: usize = 2;

/// The one store an incarnation is wired with, as the backend handle a wiring
/// point names it by and the erased handle these clauses read it through.
///
/// `EventStore` is stream-keyed, so a single instance holds the order's stream,
/// the stock item's stream and the saga's own journal side by side; splitting
/// them across instances is a deployment choice, not something the pattern
/// needs.
///
/// Both handles are kept because a delivery host below is wired from a store of
/// its own over this same journal, and naming a slot is what erases a backend —
/// so the host's assembly is named with the backend handle while everything
/// else takes the erased one.  They are one store either way: erasing a backend
/// takes that backend's own clone.
pub fn single_store() -> (InMemoryEventStore, Journal) {
    let backend = InMemoryEventStore::default();
    let journal = Journal::from(backend.clone());
    (backend, journal)
}

/// A system bound to `store`, with no saga and no delivery host on it.
///
/// The direct-dispatch clause drives the receiving aggregate itself, so it needs
/// the aggregates without the machinery that would also address them.
pub async fn spawn_system(store: Journal) -> ExampleSystem {
    let ps = ProcessSystem::new().await;
    EventSourceSystem::builder(ps)
        .with_persistence(PersistenceProvider::new(store))
        .build()
}

/// Everything one incarnation of the example owns.
///
/// `system` is kept because it is the handle to the `ProcessSystem` the
/// aggregates, the manager and the delivery hosts run on; dropping the last
/// handle early would take the incarnation down.
pub struct ReservationWorld {
    pub system: ExampleSystem,
    pub order: AggregateProxy<Order>,
    pub manager: SagaManagerProxy<ReservationSaga>,
}

/// Spawn a complete incarnation: a fresh `ProcessSystem`, the order aggregate,
/// and the saga manager observing the order's stream.
///
/// No delivery host — the clauses that need one name its stores themselves,
/// because which offset store a host is wired with is the whole subject of
/// the redelivery tests.  See [`spawn_delivery_host`], and [`ExampleSystem`]
/// for why the manager is not started through `host` here.
pub async fn spawn_world(order_id: &AggregateId, store: Journal) -> ReservationWorld {
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
/// second time over the same store delivers the order's fact event again.  The
/// saga instance it addresses drops that delivery — the position of the fact is
/// on the instance's own stream, committed by the append that carried the
/// decision — so a second incarnation decides nothing new.  What a second
/// incarnation *does* re-do is delivery, which is the delivery host's business
/// and not the manager's.
///
/// The journal and the observed stream are named by hand, which is the explicit
/// counterpart of `host`: these clauses start the manager alone, because the
/// host that carries its decisions is wired per clause over an offset store of
/// its own — see [`spawn_delivery_host`].
pub async fn spawn_manager(
    system: &ExampleSystem,
    order_id: &AggregateId,
) -> SagaManagerProxy<ReservationSaga> {
    let journal = system.persistence().event_store();
    SagaManagerProps::<ReservationSaga>::new(journal.clone(), |_| ReservationSaga)
        .observing::<OrderPlaced>(Subscription::stream(journal, order_id))
        .spawn(system.process_system())
        .await
}

// Delivery host wiring

/// The identifier every host in these tests files its progress under.
///
/// One id for every incarnation on purpose: a second host under the same id
/// resumes from where the first recorded, which is what makes "the first one
/// never recorded" the whole of the difference.
pub const TEST_DELIVERY_ID: &str = "reservation-delivery";

pub fn delivery_id() -> SagaDeliveryId {
    SagaDeliveryId::new(TEST_DELIVERY_ID)
}

/// The offset scope of the single partition every host here is spawned with.
///
/// One partition, so every saga instance's decisions are this one consumer's and
/// a test reads progress from a single key.
pub fn delivery_scope() -> OffsetScope {
    delivery_id().offset_scope(0)
}

/// The store a delivery host is wired from: `journal` as the store the saga's
/// decisions are on, `offsets` as the slot its progress is recorded in, and
/// no other slot named.
///
/// A host needs exactly those two, so nothing else is filled in — the snapshot
/// and projection slots are left [`Unprovided`] rather than named with stores no
/// clause here reads.  The journal has no parameter of its own: the backend's
/// type is written once, in the builder call that names it.
pub type ExampleDeliveryStore<Cs> = CustomStore<Unprovided, Unprovided, Cs>;

/// The supplier a host is wired from, over the two slots it needs.
///
/// Both handles are named as the backends themselves and the caller keeps its
/// own: a store's slots are logical destinations, so naming one twice is
/// cloning the handle rather than sharing it from outside.
pub fn delivery_persistence<Cs>(
    journal: &InMemoryEventStore,
    offsets: &Cs,
) -> PersistenceProvider<ExampleDeliveryStore<Cs>>
where
    Cs: OffsetStore + Clone + 'static,
{
    PersistenceProvider::new(
        CustomStore::builder()
            .eventstore(journal.clone())
            .saga_offsets(offsets.clone())
            .build(),
    )
}

/// Start a delivery host for the reservation saga: reading `journal`, recording
/// progress in `offsets`, and delivering through `system`.
///
/// The host is wired from a supplier of its own and delivers through whichever
/// system the receiving aggregates resolve on — which is what lets two
/// successive hosts with two different offset stores both reach the one
/// resident stock item.
pub async fn spawn_delivery_host<Cs>(
    system: &ExampleSystem,
    journal: &InMemoryEventStore,
    offsets: &Cs,
) -> SagaDeliveryProxy<ReservationSaga>
where
    Cs: OffsetStore + Clone + 'static,
{
    SagaDeliveryProps::<ReservationSaga, Cs>::new(&delivery_persistence(journal, offsets))
        .with_delivery_id(delivery_id())
        .with_poll_interval(POLL_INTERVAL)
        .spawn(system)
        .await
}

/// A [`OffsetStore`] whose saves never land.
///
/// This is the durable state a delivery host that died mid-record leaves behind:
/// the commands it derived may already have reached their targets, but nothing
/// says they did, so the next host resumes from the position the last one
/// recorded — here, none at all — and carries the same deliveries again.  That
/// window is the whole of why delivery is at-least-once, and it is stated once
/// on [`nitinol_saga::SagaDeliveryProps`].
///
/// `load` delegates, because a host has to be able to read whatever progress
/// *was* recorded; refusing `save` alone is what leaves exactly one thing
/// missing.  Nothing ever lifts the refusal: the host wired with this one made
/// its deliveries and is to be treated as gone, so a later success from it would
/// record progress nobody is behind.
///
/// The handle it is named over is held by value, and its clone is that handle's
/// own: a refusing view the wiring cloned has to read the positions the store
/// the caller holds does, or "the next host finds nothing recorded" would be a
/// statement about a store of its own.
#[derive(Clone)]
pub struct UnrecordedProgressStore {
    inner: InMemoryOffsetStore,
}

impl UnrecordedProgressStore {
    /// A refusing view of `inner`, which the next incarnation reads directly.
    pub fn over(inner: &InMemoryOffsetStore) -> Self {
        Self {
            inner: inner.clone(),
        }
    }
}

#[async_trait]
impl OffsetStore for UnrecordedProgressStore {
    type Tx = ();

    async fn load(&self, scope: &OffsetScope) -> Result<Option<u64>, OffsetError> {
        self.inner.load(scope).await
    }

    async fn save(
        &self,
        scope: &OffsetScope,
        sequence: u64,
        _tx: Option<&mut Self::Tx>,
    ) -> Result<(), OffsetError> {
        Err(OffsetError::Backend(Box::new(std::io::Error::other(
            format!(
                "the host recording {} is gone, so {sequence} is not written down",
                scope.as_str()
            ),
        ))))
    }
}

// Store observation

pub async fn load_stream(store: &Journal, key: &str) -> Vec<LoadedEvent> {
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
/// redelivery distinguishes "the first write was left intact" from "the record
/// was rewritten with equal content".
pub type StreamFingerprint = Vec<(u64, u64, Vec<u8>)>;

pub async fn fingerprint(store: &Journal, key: &str) -> StreamFingerprint {
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
/// repeated *delivery* distinguishable from a repeated *decision*.
pub fn requested_reservations(events: &[LoadedEvent]) -> Vec<String> {
    events
        .iter()
        .filter(|event| event.event_type == ReservationRequested::EVENT_TYPE)
        .map(|event| decode::<ReservationRequested>(event).reservation)
        .collect()
}

/// The store-wide position of the one decision on the saga's stream.
///
/// A delivery host's progress is counted in these, so this is the number a test
/// waits for an offset to reach.
pub async fn decision_position(store: &Journal, saga_key: &str) -> u64 {
    let events = load_stream(store, saga_key).await;
    let decisions: Vec<&LoadedEvent> = events
        .iter()
        .filter(|event| event.event_type == ReservationRequested::EVENT_TYPE)
        .collect();
    let [decision] = decisions.as_slice() else {
        panic!(
            "the saga must have persisted exactly one decision (stream holds {:?})",
            events
                .iter()
                .map(|event| event.event_type.to_string())
                .collect::<Vec<String>>()
        )
    };
    decision.global_sequence
}

/// Poll the stream `key` until it holds at least `expected` records.
pub async fn wait_for_records(
    store: &Journal,
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

/// What `offsets` holds for the single partition of the host under test.
pub async fn recorded_progress(offsets: &InMemoryOffsetStore) -> Option<u64> {
    offsets
        .load(&delivery_scope())
        .await
        .expect("reading an in-memory offset must succeed")
}

/// Poll `offsets` until the host has recorded progress at `want` or beyond.
///
/// This is the exact observable for "every delivery that record implied has
/// landed": a host carries a record's deliveries first and moves its offset
/// past that record only once all of them have been answered.  Waiting on it is
/// therefore a barrier rather than a guessed window — and because the host
/// `ask`s, the receiver's own append is already done by the time the offset
/// moves.
pub async fn wait_for_progress(offsets: &InMemoryOffsetStore, want: u64, label: &str) {
    let deadline = std::time::Instant::now() + DELIVERY_TIMEOUT;
    loop {
        if recorded_progress(offsets)
            .await
            .is_some_and(|at| at >= want)
        {
            return;
        }
        if std::time::Instant::now() >= deadline {
            panic!(
                "{label}: the host recorded {:?}, wanted at least {want}",
                recorded_progress(offsets).await
            );
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

// Aggregate observation

/// A reference to the stock item that owns the stream `key`.
pub fn stock_ref(system: &ExampleSystem, key: &str) -> AggregateProxy<StockItem> {
    system.aggregate_proxy::<StockItem>(AggregateId::new(key))
}
