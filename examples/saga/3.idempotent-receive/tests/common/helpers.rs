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
use nitinol_persistence::provider::{CustomStore, PersistenceProvider, Store, Unprovided};
use nitinol_persistence::store::{
    InMemoryEventStore, InMemoryOffsetStore, Journal, OffsetScope, OffsetStore,
};
use nitinol_persistence::{AggregateId, LoadQuery, LoadedEvent};
use nitinol_runtime::ProcessSystem;
use nitinol_saga::{SagaDeliveryId, SagaHostProxy, SagaManagerProps, Subscription};

use saga_idempotent_receive::order::{Order, OrderPlaced};
use saga_idempotent_receive::saga::{ReservationRequested, ReservationSaga};
use saga_idempotent_receive::stock_item::{StockItem, StockReserved};

/// The store an incarnation of this example is assembled over: one in-memory
/// journal and the slot the framework's saga attachments record their progress
/// in, with no other slot named.
///
/// `EventStore` is stream-keyed, so a single journal holds the order's stream,
/// the stock item's stream and the saga's own stream side by side; splitting
/// them across instances is a deployment choice, not something the pattern
/// needs.  The snapshot and projection slots are left [`Unprovided`] rather
/// than named with stores no clause here reads.
///
/// The offset slot keeps the backend's own type, because *which* offset store an
/// incarnation was wired with is the whole subject of the redelivery clauses:
/// one incarnation is given a store whose saves never land, the next is given
/// the store underneath it, and a test reads the recorded position back through
/// the handle it supplied.
pub type ExampleStore<Cs> = CustomStore<Unprovided, Unprovided, Cs>;

/// The system an incarnation of this example assembles.
///
/// Every store a spawn needs comes from here: the journal the aggregates and the
/// saga are tenants of, and the saga-offset slot the delivery host records in.
/// Two successive incarnations with two different offset stores are therefore
/// two systems over the one journal — which also means the second one's stock
/// item is a fresh activation that has to replay its own stream, and the pattern
/// has to hold across that too.
pub type ExampleSystem<Cs> = EventSourceSystem<ExampleStore<Cs>>;

/// The system the direct-dispatch clause assembles: the journal alone, with no
/// saga and no delivery host on it.
///
/// A store holding the journal alone *is* the journal, so it is named over a
/// [`Journal`] rather than over an assembly wrapping one.
pub type PlainSystem = EventSourceSystem<Journal>;

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
/// Both handles are kept because a system below is assembled from the backend
/// handle, and naming a slot is what erases a backend — so the assembly is named
/// with the backend handle while the clauses read the streams through the erased
/// one.  They are one store either way: erasing a backend takes that backend's
/// own clone.
pub fn single_store() -> (InMemoryEventStore, Journal) {
    let backend = InMemoryEventStore::default();
    let journal = Journal::from(backend.clone());
    (backend, journal)
}

/// A system bound to `store`, with no saga and no delivery host on it.
///
/// The direct-dispatch clause drives the receiving aggregate itself, so it needs
/// the aggregates without the machinery that would also address them.
pub async fn spawn_system(store: Journal) -> PlainSystem {
    let ps = ProcessSystem::new().await;
    EventSourceSystem::builder(ps)
        .with_persistence(PersistenceProvider::new(store))
        .build()
}

/// Everything one incarnation of the example owns.
///
/// `system` is kept because it is the handle to the `ProcessSystem` the
/// aggregates, the manager and the delivery host run on; dropping the last
/// handle early would take the incarnation down.
pub struct ReservationWorld<Cs> {
    pub system: ExampleSystem<Cs>,
    pub order: AggregateProxy<Order>,
    pub host: SagaHostProxy<ReservationSaga>,
}

/// Spawn a complete incarnation: a fresh `ProcessSystem` over `journal` and
/// `offsets`, the order aggregate, and the saga hosted on it.
///
/// One spawn starts both halves — the manager that decides and the delivery host
/// that carries the decision out — because a saga whose decisions nobody carries
/// out is not a saga.  Which offset store the host records in is the whole
/// subject of the redelivery clauses, and it is named here, as the slot of the
/// system the saga is spawned on.
///
/// The manager's cursor starts at `after: 0` and lives in memory, so a second
/// incarnation over the same journal delivers the order's fact event again.  The
/// saga instance it addresses drops that delivery — the position of the fact is
/// on the instance's own stream, committed by the append that carried the
/// decision — so a second incarnation decides nothing new.  What a second
/// incarnation *does* re-do is delivery.
pub async fn spawn_world<Cs>(
    order_id: &AggregateId,
    journal: &InMemoryEventStore,
    offsets: &Cs,
) -> ReservationWorld<Cs>
where
    Cs: OffsetStore + Clone + 'static,
{
    let system = EventSourceSystem::builder(ProcessSystem::new().await)
        .with_persistence(example_persistence(journal, offsets))
        .build();

    let order = system.spawn_aggregate::<Order>(order_id.clone()).await;

    let host = SagaManagerProps::<ReservationSaga>::new(|_| ReservationSaga)
        .observing::<OrderPlaced>(Subscription::stream(order_id))
        .with_delivery_id(delivery_id())
        .with_delivery_poll_interval(POLL_INTERVAL)
        .spawn(&system)
        .await;

    ReservationWorld {
        system,
        order,
        host,
    }
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

/// The supplier an incarnation is assembled from, over the two slots it needs.
///
/// Both handles are named as the backends themselves and the caller keeps its
/// own: a store's slots are logical destinations, so naming one twice is
/// cloning the handle rather than sharing it from outside.
pub fn example_persistence<Cs>(
    journal: &InMemoryEventStore,
    offsets: &Cs,
) -> PersistenceProvider<ExampleStore<Cs>>
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

/// A reference to the stock item that owns the stream `key`, on `system`.
pub fn stock_ref<St: Store>(
    system: &EventSourceSystem<St>,
    key: &str,
) -> AggregateProxy<StockItem> {
    system.aggregate_proxy::<StockItem>(AggregateId::new(key))
}
