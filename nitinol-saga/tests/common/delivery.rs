//! Shared fixtures for the saga delivery host. Each test binary compiles this
//! module in full but uses only the subset it needs, so per-binary dead code is
//! expected.
//!
//! The decision event below carries both the target stream key and the
//! operation id, so a mapping declared on the saga type needs nothing but the
//! event itself. That is what lets every test here seed a saga journal by
//! appending rows directly rather than by running a saga incarnation: the host
//! reads the store, so the store is the whole of its input.

#![allow(dead_code)]

use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use bytes::Bytes;
use futures_util::TryStreamExt;
use serde::{Deserialize, Serialize};

use nitinol_eventsource::codec::Codec;
use nitinol_eventsource::system::EventSourceSystem;
use nitinol_eventsource::{Aggregate, AggregateProxy, Decider, Decision, Event, Never, Query};
use nitinol_persistence::error::{AppendError, LoadError, OffsetError};
use nitinol_persistence::provider::{CustomStore, PersistenceProvider, Unprovided};
use nitinol_persistence::store::{
    EventStore, EventStream, InMemoryEventStore, Journal, OffsetScope, OffsetStore,
};
use nitinol_persistence::{
    AggregateId, AppendOutcome, AppendingEvent, EventType, Family, LoadQuery, LoadedEvent, TypeName,
};
use nitinol_runtime::ProcessSystem;
use nitinol_saga::{
    Saga, SagaContext, SagaDelivery, SagaDeliveryId, SagaDeliveryProxy, SagaEffect, SagaId,
    SagaManagerProps, SagaSubscribe, Subscription,
};

/// How long a probe waits for work the host performs on its own poll loop.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// How often a probe re-reads what it is waiting for.
pub const PROBE_INTERVAL: Duration = Duration::from_millis(20);

/// Poll interval every test that is not about the interval itself configures.
///
/// Short enough that a probe sees several ticks inside [`PROBE_TIMEOUT`], and
/// named so that a test's waiting is not tuned against a hidden default.
pub const TEST_POLL_INTERVAL: Duration = Duration::from_millis(25);

/// Why a fixture lock cannot be poisoned: every holder does a read or a push
/// and returns.
pub const FIXTURE_LOCK: &str = "a fixture lock guards only a read or a push, so no holder panics";

pub struct DeliveryCodec;

impl<E: Serialize + for<'de> Deserialize<'de>> Codec<E> for DeliveryCodec {
    type Error = serde_json::Error;

    fn encode(event: &E) -> Result<Bytes, Self::Error> {
        serde_json::to_vec(event).map(Bytes::from)
    }

    fn decode(payload: &[u8]) -> Result<E, Self::Error> {
        serde_json::from_slice(payload)
    }
}

/// The saga's own decision event — the stream the delivery host consumes.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Decided {
    /// Stream key of the aggregate the decision is addressed to.
    pub target: String,
    /// The operation id the receiver recognises a repeat by.
    pub operation: String,
}

impl Event for Decided {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("delivery.host"), TypeName::new("Decided"));
    type Codec = DeliveryCodec;
}

/// A row of another event type sharing the saga's stream.
///
/// The host's subscription must leave it out, and the gaps it leaves in the
/// returned records are what the cursor has to survive.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Noted;

impl Event for Noted {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("delivery.host"), TypeName::new("Noted"));
    type Codec = DeliveryCodec;
}

/// The one input the fixture sagas declare. Nothing drives it: these tests
/// seed decisions directly, and the alphabet exists because [`Saga`] names one.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Triggered;

impl Event for Triggered {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("delivery.host"), TypeName::new("Triggered"));
    type Codec = DeliveryCodec;
}

#[derive(SagaSubscribe)]
pub enum Observed {
    Trigger(Triggered),
}

/// What the receiving aggregate writes down when it carries an operation out.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Recorded {
    pub operation: String,
}

impl Event for Recorded {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("delivery.host"), TypeName::new("Recorded"));
    type Codec = DeliveryCodec;
}

/// The receiving aggregate — an ordinary `Aggregate` + `Decider` pair, with no
/// delivery-host surface implemented on it.
#[derive(Default)]
pub struct Ledger {
    operations: Vec<String>,
}

impl Aggregate for Ledger {
    type Event = Recorded;
    type Recover = Never;

    fn apply(&mut self, event: Recorded) {
        self.operations.push(event.operation);
    }
}

/// The command the mapping produces.
///
/// Carries neither `Clone` nor `Serialize`: the host persists no copy of a
/// command, and `ask` asks for neither.
pub struct Record {
    pub operation: String,
}

/// Idempotent receive: the operation id is a domain fact, so a redelivered
/// command is accepted with nothing written.
impl Decider<Record> for Ledger {
    type Output = ();
    type Rejection = std::convert::Infallible;

    fn decide(&self, cmd: Record) -> Decision<Recorded, (), Self::Rejection> {
        if self.operations.contains(&cmd.operation) {
            return Decision::persist(Vec::new()).output(());
        }
        Decision::persist(vec![Recorded {
            operation: cmd.operation,
        }])
        .output(())
    }
}

/// Every operation this ledger has carried out, in the order it did.
pub struct Operations;

impl Query<Operations> for Ledger {
    type Response = Vec<String>;
    type Error = std::convert::Infallible;

    fn query(&self, _msg: Operations) -> Result<Vec<String>, Self::Error> {
        Ok(self.operations.clone())
    }
}

/// The verdict [`Refuser`] reaches on every command.
#[derive(Debug)]
pub struct Refused;

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "the receiver refuses this command")
    }
}

impl std::error::Error for Refused {}

/// A receiver whose decider refuses every command it is handed. Re-issuing the
/// command reproduces the verdict, so a host that observes the refusal has to
/// hold rather than move on.
#[derive(Default)]
pub struct Refuser;

impl Aggregate for Refuser {
    type Event = Recorded;
    type Recover = Never;

    fn apply(&mut self, _event: Recorded) {}
}

impl Decider<Record> for Refuser {
    type Output = ();
    type Rejection = Refused;

    fn decide(&self, _cmd: Record) -> Decision<Recorded, (), Refused> {
        Decision::reject(Refused)
    }
}

/// Maps one decision to one command against a [`Ledger`].
#[derive(Default)]
pub struct DeliverySaga;

#[async_trait]
impl Saga for DeliverySaga {
    type Subscribe = Observed;
    type Event = Decided;
    type Error = std::convert::Infallible;

    fn correlate(_input: &Self::Subscribe) -> Option<SagaId> {
        None
    }

    fn dispatch(event: &Self::Event) -> Vec<SagaDelivery> {
        vec![SagaDelivery::new::<Ledger, Record>(
            AggregateId::new(event.target.clone()),
            Record {
                operation: event.operation.clone(),
            },
        )]
    }

    async fn handle(
        &mut self,
        _input: Self::Subscribe,
        _ctx: &mut SagaContext,
    ) -> Result<SagaEffect<Self::Event>, Self::Error> {
        Ok(SagaEffect::None)
    }
}

/// Maps one decision to one command against a [`Refuser`].
#[derive(Default)]
pub struct RefusedSaga;

#[async_trait]
impl Saga for RefusedSaga {
    type Subscribe = Observed;
    type Event = Decided;
    type Error = std::convert::Infallible;

    fn correlate(_input: &Self::Subscribe) -> Option<SagaId> {
        None
    }

    fn dispatch(event: &Self::Event) -> Vec<SagaDelivery> {
        vec![SagaDelivery::new::<Refuser, Record>(
            AggregateId::new(event.target.clone()),
            Record {
                operation: event.operation.clone(),
            },
        )]
    }

    async fn handle(
        &mut self,
        _input: Self::Subscribe,
        _ctx: &mut SagaContext,
    ) -> Result<SagaEffect<Self::Event>, Self::Error> {
        Ok(SagaEffect::None)
    }
}

/// Two deliveries from one decision: the first lands on a [`Ledger`], the
/// second is refused. One decision the host cannot finish is what the cursor
/// must not move past, even though part of it succeeded.
#[derive(Default)]
pub struct PartlyRefusedSaga;

/// Stream key suffix the second, refused delivery of [`PartlyRefusedSaga`] is
/// addressed to.
pub const REFUSED_SUFFIX: &str = "-refused";

#[async_trait]
impl Saga for PartlyRefusedSaga {
    type Subscribe = Observed;
    type Event = Decided;
    type Error = std::convert::Infallible;

    fn correlate(_input: &Self::Subscribe) -> Option<SagaId> {
        None
    }

    fn dispatch(event: &Self::Event) -> Vec<SagaDelivery> {
        vec![
            SagaDelivery::new::<Ledger, Record>(
                AggregateId::new(event.target.clone()),
                Record {
                    operation: event.operation.clone(),
                },
            ),
            SagaDelivery::new::<Refuser, Record>(
                AggregateId::new(format!("{}{REFUSED_SUFFIX}", event.target)),
                Record {
                    operation: event.operation.clone(),
                },
            ),
        ]
    }

    async fn handle(
        &mut self,
        _input: Self::Subscribe,
        _ctx: &mut SagaContext,
    ) -> Result<SagaEffect<Self::Event>, Self::Error> {
        Ok(SagaEffect::None)
    }
}

/// Routes by what the decision says: a target named with [`REFUSED_SUFFIX`]
/// goes to a [`Refuser`], anything else to a [`Ledger`].
///
/// One saga type is what makes a partition-local stall observable: both
/// instances are the same type on the same subscription, so the only thing
/// separating them is the partition their stream key falls in.
#[derive(Default)]
pub struct SelectiveSaga;

#[async_trait]
impl Saga for SelectiveSaga {
    type Subscribe = Observed;
    type Event = Decided;
    type Error = std::convert::Infallible;

    fn correlate(_input: &Self::Subscribe) -> Option<SagaId> {
        None
    }

    fn dispatch(event: &Self::Event) -> Vec<SagaDelivery> {
        let target = AggregateId::new(event.target.clone());
        let command = Record {
            operation: event.operation.clone(),
        };
        if event.target.ends_with(REFUSED_SUFFIX) {
            vec![SagaDelivery::new::<Refuser, Record>(target, command)]
        } else {
            vec![SagaDelivery::new::<Ledger, Record>(target, command)]
        }
    }

    async fn handle(
        &mut self,
        _input: Self::Subscribe,
        _ctx: &mut SagaContext,
    ) -> Result<SagaEffect<Self::Event>, Self::Error> {
        Ok(SagaEffect::None)
    }
}

/// An [`EventStore`] whose appends to one named stream always fail with a
/// backend failure, and which passes everything else to the store it wraps.
///
/// A receiver whose own stream is the refused one accepts a command and then
/// cannot record it, which is a failure of the machinery rather than a verdict
/// on the command — the transient half of the stall. The rest of the store
/// still works, because a system distributes one journal: the saga's own
/// decisions and the receiver's stream are tenants of it, and only the
/// receiver's half is meant to be unwritable.
///
/// `Clone` is the inner store's handle plus the key, so every handle refuses
/// the same stream and reads the same rows.
#[derive(Clone)]
pub struct AppendRefusingEventStore {
    inner: Journal,
    /// The stream whose appends are refused.
    refused: String,
}

impl AppendRefusingEventStore {
    /// Wrap `inner`, refusing every append addressed to `refused`.
    pub fn refusing(inner: impl Into<Journal>, refused: &AggregateId) -> Self {
        Self {
            inner: inner.into(),
            refused: refused.as_str().to_owned(),
        }
    }
}

#[async_trait]
impl EventStore for AppendRefusingEventStore {
    async fn append(
        &self,
        key: &str,
        events: Vec<AppendingEvent>,
    ) -> Result<AppendOutcome, AppendError> {
        if key == self.refused {
            return Err(AppendError::Backend(Box::new(std::io::Error::other(
                "the fixture store cannot record anything for this stream",
            ))));
        }
        self.inner.append(key, events).await
    }

    async fn load(&self, query: LoadQuery) -> Result<EventStream<'_>, LoadError> {
        self.inner.load(query).await
    }
}

/// A saga that declares no mapping at all, so the trait's default answers for
/// it. An existing saga compiles untouched, and delivers nothing.
#[derive(Default)]
pub struct SilentSaga;

#[async_trait]
impl Saga for SilentSaga {
    type Subscribe = Observed;
    type Event = Decided;
    type Error = std::convert::Infallible;

    fn correlate(_input: &Self::Subscribe) -> Option<SagaId> {
        None
    }

    async fn handle(
        &mut self,
        _input: Self::Subscribe,
        _ctx: &mut SagaContext,
    ) -> Result<SagaEffect<Self::Event>, Self::Error> {
        Ok(SagaEffect::None)
    }
}

/// The identifier every test here names its host with, except the one test
/// whose subject is the identifier.
pub const TEST_DELIVERY_ID: &str = "delivery-under-test";

pub fn delivery_id() -> SagaDeliveryId {
    SagaDeliveryId::new(TEST_DELIVERY_ID)
}

/// The offset scope of partition `partition` of the host spawned by
/// [`spawn_host`].
pub fn scope_of(partition: usize) -> OffsetScope {
    delivery_id().offset_scope(partition)
}

pub fn partition_count(n: usize) -> NonZeroUsize {
    NonZeroUsize::new(n).expect("a fixture partition count is not zero")
}

/// The store a delivery host is wired from here: the journal the saga's
/// decisions are on and the receiving aggregates are tenants of, plus
/// `offsets` as the slot the host records in, and no other slot named.
///
/// A host needs exactly those two, so a store with nothing else on it is what
/// the fixtures assemble — the projection and snapshot slots are left
/// unprovided rather than filled with stores no clause here reads.
///
/// One journal rather than two: a host reads the journal the system it was
/// spawned on distributes, and there is no per-spawn store to name a second
/// one with. The rows a host reads and the rows its deliveries write stay apart
/// by stream key, which is what the clauses here assert on.
///
/// Each handle is named as the backend's own value: the journal is erased where
/// the builder takes it, and the offset slot keeps its type so the host's
/// progress is read back through the very handle it was wired with.
pub type DeliveryStore = CustomStore<Unprovided, Unprovided, RecordingOffsetStore>;

/// The supplier the delivery fixtures wire a host from.
pub type DeliveryPersistence = PersistenceProvider<DeliveryStore>;

/// The system the delivery fixtures spawn onto.
pub type DeliverySystem = EventSourceSystem<DeliveryStore>;

pub fn delivery_persistence<E>(journal: &E, offsets: &RecordingOffsetStore) -> DeliveryPersistence
where
    E: EventStore + Clone + 'static,
{
    PersistenceProvider::new(
        CustomStore::builder()
            .eventstore(journal.clone())
            .saga_offsets(offsets.clone())
            .build(),
    )
}

/// A system over `journal` and `offsets`.
pub async fn delivery_system<E>(journal: &E, offsets: &RecordingOffsetStore) -> DeliverySystem
where
    E: EventStore + Clone + 'static,
{
    EventSourceSystem::builder(ProcessSystem::new().await)
        .with_persistence(delivery_persistence(journal, offsets))
        .build()
}

/// A system over a fresh in-memory journal and `offsets`, and the backend handle
/// that journal was named with.
///
/// The handle is answered as the backend's own value rather than as an erased
/// journal, so a fixture can both seed a saga's decisions through it and read
/// the rows the system's deliveries wrote.
pub async fn in_memory_system(
    offsets: &RecordingOffsetStore,
) -> (DeliverySystem, InMemoryEventStore) {
    let store = InMemoryEventStore::default();
    let system = delivery_system(&store, offsets).await;
    (system, store)
}

/// The stream key the fixture sagas' one declared source is bound to.
///
/// Nothing ever writes a [`Triggered`] row: these tests seed decisions
/// directly, and the alphabet exists only because [`Saga`] names one. The
/// binding is still required at the wiring, so it names a stream of its own
/// rather than one a clause reads.
pub const TRIGGER_KEY: &str = "delivery-fixture-unused-trigger";

/// Start the saga `S` on `system`, with `partitions` delivery partitions, and
/// answer the delivery half of what it started.
///
/// One spawn starts both halves — a saga whose decisions nobody carries out is
/// not a saga — and the manager is left running: dropping its handle stops
/// nothing, and the fixture sagas correlate to no instance, so the manager
/// fronts nothing while the host does the work these clauses read.
pub async fn spawn_host<S>(system: &DeliverySystem, partitions: usize) -> SagaDeliveryProxy<S>
where
    S: Saga<Subscribe = Observed> + Default,
{
    SagaManagerProps::<S>::new(|_| S::default())
        .observing::<Triggered>(Subscription::stream(TRIGGER_KEY))
        .with_delivery_id(delivery_id())
        .with_partitions(partition_count(partitions))
        .with_delivery_poll_interval(TEST_POLL_INTERVAL)
        .spawn(system)
        .await
        .delivery()
        .clone()
}

/// Append one row to `stream` under the identity `E` declares.
pub async fn append_row<E: Event + Serialize>(
    store: &dyn EventStore,
    stream: &str,
    sequence: u64,
    event: &E,
) -> u64 {
    let payload = serde_json::to_vec(event)
        .map(Bytes::from)
        .expect("a fixture event encodes");
    let outcome = store
        .append(
            stream,
            vec![AppendingEvent {
                sequence,
                event_type: E::EVENT_TYPE,
                payload,
                occurred_at: jiff::Timestamp::now(),
            }],
        )
        .await
        .expect("appending a fixture row succeeds");
    *outcome
        .assigned_sequences
        .first()
        .expect("a one-event append is assigned one global sequence")
}

/// Append one decision to a saga's own stream and answer its global position.
pub async fn append_decided(
    store: &dyn EventStore,
    saga_id: &SagaId,
    sequence: u64,
    target: &str,
    operation: &str,
) -> u64 {
    append_row(
        store,
        saga_id.as_str(),
        sequence,
        &Decided {
            target: target.to_owned(),
            operation: operation.to_owned(),
        },
    )
    .await
}

/// Append one row of the other event type to a saga's own stream.
pub async fn append_noted(store: &dyn EventStore, saga_id: &SagaId, sequence: u64) -> u64 {
    append_row(store, saga_id.as_str(), sequence, &Noted).await
}

/// Every row of `stream`, in ascending global order.
pub async fn stream_rows(store: &dyn EventStore, stream: &str) -> Vec<LoadedEvent> {
    store
        .load(LoadQuery::by_stream(stream))
        .await
        .expect("loading a fixture stream succeeds")
        .try_collect()
        .await
        .expect("collecting a fixture stream succeeds")
}

/// The operations a ledger has carried out.
pub async fn operations_of(proxy: &AggregateProxy<Ledger>) -> Vec<String> {
    proxy
        .exec(Operations)
        .await
        .expect("querying a ledger succeeds")
}

/// Wait until `proxy` has carried out at least `want` operations, then answer
/// them. Panics with `label` when the deadline passes.
pub async fn wait_until_operations(
    proxy: &AggregateProxy<Ledger>,
    want: usize,
    label: &str,
) -> Vec<String> {
    let deadline = Instant::now() + PROBE_TIMEOUT;
    loop {
        let operations = operations_of(proxy).await;
        if operations.len() >= want {
            return operations;
        }
        if Instant::now() >= deadline {
            panic!("{label}: saw {operations:?}, wanted at least {want} operations");
        }
        tokio::time::sleep(PROBE_INTERVAL).await;
    }
}

/// Wait until the store has recorded at least `want` load calls, then answer
/// every query it has seen.
pub async fn wait_until_loads(
    store: &RecordingEventStore,
    want: usize,
    label: &str,
) -> Vec<LoadQuery> {
    let deadline = Instant::now() + PROBE_TIMEOUT;
    loop {
        let queries = store.queries();
        if queries.len() >= want {
            return queries;
        }
        if Instant::now() >= deadline {
            panic!("{label}: saw {} load calls, wanted {want}", queries.len());
        }
        tokio::time::sleep(PROBE_INTERVAL).await;
    }
}

/// How a [`RecordingEventStore`] answers the next load call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoadFault {
    /// Answer the query from the inner store.
    None,
    /// Panic, so whatever is driving the load is taken down by supervision.
    Panic,
}

/// An [`EventStore`] that records every query it is handed, when it was handed
/// it, and can be told to panic on a chosen call.
///
/// Its `Clone` is the handle's: the record of calls and the faults still to be
/// answered are the one region behind every handle, so a fixture that names
/// this store in a wiring and keeps a handle of its own reads what the wiring
/// asked for.
#[derive(Clone)]
pub struct RecordingEventStore {
    inner: Journal,
    calls: Arc<Mutex<Vec<(LoadQuery, Instant)>>>,
    /// Faults keyed by 1-based load-call number.
    faults: Arc<Mutex<HashMap<usize, LoadFault>>>,
}

impl RecordingEventStore {
    pub fn new(inner: impl Into<Journal>) -> Self {
        Self {
            inner: inner.into(),
            calls: Arc::new(Mutex::new(Vec::new())),
            faults: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Make the `nth` load call (1-based) answer with `fault`.
    pub fn fault_on(&self, nth: usize, fault: LoadFault) {
        self.faults.lock().expect(FIXTURE_LOCK).insert(nth, fault);
    }

    /// Every query a delivery host handed this store, in call order.
    ///
    /// A host asks for the rows of the saga's own decision type, and the
    /// journal it reads is the one the receiving aggregates replay from and the
    /// manager polls its sources on — so what a clause about *the host* can
    /// read is the subset of calls carrying that event type, not every call the
    /// store saw.
    pub fn queries(&self) -> Vec<LoadQuery> {
        self.host_calls()
            .into_iter()
            .map(|(query, _)| query)
            .collect()
    }

    /// The gaps between consecutive delivery-host load calls, in call order.
    pub fn load_gaps(&self) -> Vec<Duration> {
        self.host_calls()
            .windows(2)
            .map(|pair| pair[1].1.saturating_duration_since(pair[0].1))
            .collect()
    }

    pub fn load_count(&self) -> usize {
        self.host_calls().len()
    }

    /// The calls a delivery host made, as `(query, when)` in call order.
    fn host_calls(&self) -> Vec<(LoadQuery, Instant)> {
        let decided = Decided::EVENT_TYPE.to_path();
        self.calls
            .lock()
            .expect(FIXTURE_LOCK)
            .iter()
            .filter(|(query, _)| query.event_type_prefix.as_ref() == Some(&decided))
            .cloned()
            .collect()
    }
}

#[async_trait]
impl EventStore for RecordingEventStore {
    async fn append(
        &self,
        key: &str,
        events: Vec<AppendingEvent>,
    ) -> Result<AppendOutcome, AppendError> {
        self.inner.append(key, events).await
    }

    async fn load(&self, query: LoadQuery) -> Result<EventStream<'_>, LoadError> {
        let nth = {
            let mut calls = self.calls.lock().expect(FIXTURE_LOCK);
            calls.push((query.clone(), Instant::now()));
            calls.len()
        };
        let fault = self
            .faults
            .lock()
            .expect(FIXTURE_LOCK)
            .get(&nth)
            .copied()
            .unwrap_or(LoadFault::None);
        match fault {
            LoadFault::None => self.inner.load(query).await,
            LoadFault::Panic => panic!("the fixture store panics on load call {nth}"),
        }
    }
}

/// A [`OffsetStore`] keyed by the rendered scope key, which is the whole of
/// what a scope hands a store.
///
/// Saves only ever move a key forward, so the reference implementation and a
/// forward-only backend answer a correct host identically; a save that would
/// move a key backwards is recorded as refused rather than applied.
///
/// Its `Clone` is the handle's: the positions, the record of saves and the
/// failure budget are the one region behind every handle, so the slot a store
/// is assembled with and the handle a fixture keeps are one store.
#[derive(Clone)]
pub struct RecordingOffsetStore {
    state: Arc<Mutex<HashMap<String, u64>>>,
    saves: Arc<Mutex<Vec<(String, u64)>>>,
    /// Every save this store answered with an error, in call order.
    ///
    /// Kept apart from `saves` so a test can wait until a refusal has actually
    /// reached the host, rather than inferring one from a position that is not
    /// there yet — which a slow first tick would read the same way.
    refusals: Arc<Mutex<Vec<(String, u64)>>>,
    /// Number of save calls still to be answered with a failure.
    save_failures: Arc<AtomicUsize>,
}

impl RecordingOffsetStore {
    pub fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(HashMap::new())),
            saves: Arc::new(Mutex::new(Vec::new())),
            refusals: Arc::new(Mutex::new(Vec::new())),
            save_failures: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// Fail the next `n` save calls.
    pub fn fail_next_saves(&self, n: usize) {
        self.save_failures.store(n, Ordering::SeqCst);
    }

    /// The position stored for `scope`, as an operator or a second consumer
    /// would read it.
    pub fn stored(&self, scope: &OffsetScope) -> Option<u64> {
        self.state
            .lock()
            .expect(FIXTURE_LOCK)
            .get(scope.as_str())
            .copied()
    }

    /// Write `sequence` for `scope` without going through the host, so a test
    /// can present a stored position the host did not put there.
    pub fn put(&self, scope: &OffsetScope, sequence: u64) {
        self.state
            .lock()
            .expect(FIXTURE_LOCK)
            .insert(scope.as_str().to_owned(), sequence);
    }

    /// Every save the store was asked for, as `(key, sequence)` in call order,
    /// including the ones it answered with a failure.
    pub fn save_calls(&self) -> Vec<(String, u64)> {
        self.saves.lock().expect(FIXTURE_LOCK).clone()
    }

    /// Every save the store answered with an error, as `(key, sequence)` in
    /// call order.
    pub fn refused_saves(&self) -> Vec<(String, u64)> {
        self.refusals.lock().expect(FIXTURE_LOCK).clone()
    }

    /// Every key this store holds a position for.
    pub fn keys(&self) -> Vec<String> {
        let mut keys: Vec<String> = self
            .state
            .lock()
            .expect(FIXTURE_LOCK)
            .keys()
            .cloned()
            .collect();
        keys.sort();
        keys
    }
}

#[async_trait]
impl OffsetStore for RecordingOffsetStore {
    type Tx = ();

    async fn load(&self, scope: &OffsetScope) -> Result<Option<u64>, OffsetError> {
        Ok(self.stored(scope))
    }

    async fn save(
        &self,
        scope: &OffsetScope,
        sequence: u64,
        _tx: Option<&mut Self::Tx>,
    ) -> Result<(), OffsetError> {
        let key = scope.as_str().to_owned();
        self.saves
            .lock()
            .expect(FIXTURE_LOCK)
            .push((key.clone(), sequence));
        if take_one(&self.save_failures) {
            return Err(self.refuse(key, sequence, "the fixture offset store refuses this save"));
        }
        let mut state = self.state.lock().expect(FIXTURE_LOCK);
        let held = state.get(&key).copied().unwrap_or(0);
        if sequence <= held {
            // Released first so the two fixture locks are never held at once.
            drop(state);
            return Err(self.refuse(key, sequence, "an offset key only moves forward"));
        }
        state.insert(key, sequence);
        Ok(())
    }
}

impl RecordingOffsetStore {
    /// Record that this save was refused, and answer the refusal.
    fn refuse(&self, key: String, sequence: u64, reason: &'static str) -> OffsetError {
        self.refusals
            .lock()
            .expect(FIXTURE_LOCK)
            .push((key, sequence));
        OffsetError::Backend(Box::new(std::io::Error::other(reason)))
    }
}

/// Consume one unit of a failure budget, answering whether this call is one of
/// the failing ones.
fn take_one(budget: &AtomicUsize) -> bool {
    budget
        .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |left| {
            left.checked_sub(1)
        })
        .is_ok()
}

/// Wait until the store has answered at least `want` saves with a refusal,
/// then answer every refusal it has made. Panics with `label` when the deadline
/// passes.
///
/// What a test needs before it can judge what a host does *after* a refusal:
/// the refusal has to have reached the host, and a store told to refuse has not
/// refused anything until it has been asked.
pub async fn wait_until_refused(
    offsets: &RecordingOffsetStore,
    want: usize,
    label: &str,
) -> Vec<(String, u64)> {
    let deadline = Instant::now() + PROBE_TIMEOUT;
    loop {
        let refusals = offsets.refused_saves();
        if refusals.len() >= want {
            return refusals;
        }
        if Instant::now() >= deadline {
            panic!(
                "{label}: the store refused {} saves, wanted at least {want}",
                refusals.len()
            );
        }
        tokio::time::sleep(PROBE_INTERVAL).await;
    }
}

/// Wait until `scope` holds at least `want`, then answer what it holds.
pub async fn wait_until_stored(
    offsets: &RecordingOffsetStore,
    scope: &OffsetScope,
    want: u64,
    label: &str,
) -> u64 {
    let deadline = Instant::now() + PROBE_TIMEOUT;
    loop {
        if let Some(held) = offsets.stored(scope) {
            if held >= want {
                return held;
            }
        }
        if Instant::now() >= deadline {
            panic!(
                "{label}: {} holds {:?}, wanted at least {want}",
                scope.as_str(),
                offsets.stored(scope)
            );
        }
        tokio::time::sleep(PROBE_INTERVAL).await;
    }
}

/// Give the host a window in which it would have made progress if it were
/// going to, so a test can assert that it did not.
///
/// Sized against [`TEST_POLL_INTERVAL`] rather than against a bare constant: it
/// has to cover several ticks of the loop under test.
pub async fn settle() {
    tokio::time::sleep(TEST_POLL_INTERVAL * 20).await;
}
