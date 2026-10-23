//! Shared fixtures for the manager's per-source, per-partition offsets. Each
//! test binary compiles this module in full but uses only the subset it needs,
//! so per-binary dead code is expected.
//!
//! A binary that includes this module also includes `common/delivery.rs` as
//! `delivery`: the recording offset store and the row-appending helper live
//! there, and the manager records in the very slot the delivery host does.
//!
//! The saga below correlates every record to the instance the record names, so
//! a test picks the partition a record falls in by picking the instance it is
//! addressed to. It decides nothing — every input it accepts is settled by its
//! position alone — so no decision row reaches the delivery host, and the only
//! positions the offset slot ends up holding are the manager's.

#![allow(dead_code)]

use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use nitinol_eventsource::Event;
use nitinol_persistence::error::{AppendError, LoadError};
use nitinol_persistence::store::{EventStore, EventStream, InMemoryEventStore, OffsetScope};
use nitinol_persistence::{AppendOutcome, AppendingEvent, EventType, Family, LoadQuery, TypeName};
use nitinol_runtime::ident::Pid;
use nitinol_runtime::ProcessSystem;
use nitinol_saga::{
    Saga, SagaContext, SagaDeliveryId, SagaEffect, SagaHostProxy, SagaId, SagaManagerProps,
    SagaSubscribe, Subscription,
};

use crate::delivery::{
    append_row, DeliveryCodec, DeliverySystem, FIXTURE_LOCK, PROBE_INTERVAL, PROBE_TIMEOUT,
    TEST_POLL_INTERVAL,
};

// The key a manager's progress is recorded under

/// The part every manager offset is recorded under, beside the delivery host's
/// `saga-delivery` in one shared table.
pub const MANAGER_PART: &str = "saga-manager";

/// The scope partition `partition` of the manager hosted as `host_id` records
/// its progress through the declared source `source` under.
///
/// The name is the identifier's byte length, the identifier, and the source's
/// canonical path, joined by `:`. Neither an identifier nor a path forbids `:`,
/// so the length is what splits the two back apart: a pair is recovered from
/// the name alone, which is what makes the key one-to-one in (identifier,
/// source). The key persists across releases, so it is written out here rather
/// than derived from anything the crate exposes.
pub fn manager_scope(host_id: &str, source: EventType, partition: usize) -> OffsetScope {
    OffsetScope::framework_owned(
        MANAGER_PART,
        &format!("{}:{}:{}", host_id.len(), host_id, source.to_path()),
        partition,
    )
}

/// The identifier every manager here is hosted as, except where the default
/// identifier is the subject.
pub const HOST_ID: &str = "manager-offset-under-test";

pub fn host_id() -> SagaDeliveryId {
    SagaDeliveryId::new(HOST_ID)
}

/// The scope of partition `partition` of the [`Placed`] source of a host named
/// [`HOST_ID`].
pub fn placed_scope(partition: usize) -> OffsetScope {
    manager_scope(HOST_ID, Placed::EVENT_TYPE, partition)
}

/// The scope of partition `partition` of the [`Shipped`] source of a host named
/// [`HOST_ID`].
pub fn shipped_scope(partition: usize) -> OffsetScope {
    manager_scope(HOST_ID, Shipped::EVENT_TYPE, partition)
}

pub fn partitions(n: usize) -> NonZeroUsize {
    NonZeroUsize::new(n).expect("a fixture partition count is not zero")
}

// Domain

/// One declared source. The instance a record belongs to is named on it.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Placed {
    pub saga: String,
    pub note: String,
}

impl Event for Placed {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("manager.offset"), TypeName::new("Placed"));
    type Codec = DeliveryCodec;
}

/// The other declared source, so a saga here observes two independently.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Shipped {
    pub saga: String,
    pub note: String,
}

impl Event for Shipped {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("manager.offset"), TypeName::new("Shipped"));
    type Codec = DeliveryCodec;
}

/// The saga's own decision type. Nothing here ever decides one; the type exists
/// because [`Saga`] names one, and its path is the default host identifier.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Acted;

impl Event for Acted {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("manager.offset"), TypeName::new("Acted"));
    type Codec = DeliveryCodec;
}

#[derive(SagaSubscribe)]
pub enum Upstream {
    Placed(Placed),
    Shipped(Shipped),
}

/// The note a record carries when its `handle` must refuse it.
pub const REFUSE: &str = "refuse";

/// What one `handle` invocation saw: the source stream key and sequence of the
/// record that reached it.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Seen {
    pub source: String,
    pub sequence: u64,
}

pub type Handled = Arc<Mutex<Vec<Seen>>>;

pub fn handled() -> Handled {
    Arc::new(Mutex::new(Vec::new()))
}

/// How many instances the host constructed — one per spawn, so a count that
/// does not move says no instance was stopped and replaced.
pub type Made = Arc<AtomicUsize>;

pub fn made() -> Made {
    Arc::new(AtomicUsize::new(0))
}

/// The verdict `handle` reaches on a record noted [`REFUSE`].
#[derive(Debug)]
pub struct Refused;

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "the fixture saga refuses this record")
    }
}

impl std::error::Error for Refused {}

pub struct OffsetSaga {
    handled: Handled,
}

impl OffsetSaga {
    /// An instance recording every record that reaches its `handle` into
    /// `handled`.
    pub fn recording(handled: &Handled) -> Self {
        Self {
            handled: Arc::clone(handled),
        }
    }
}

#[async_trait]
impl Saga for OffsetSaga {
    type Subscribe = Upstream;
    type Event = Acted;
    type Error = Refused;

    fn correlate(input: &Self::Subscribe) -> Option<SagaId> {
        match input {
            Upstream::Placed(placed) => Some(SagaId::new(placed.saga.clone())),
            Upstream::Shipped(shipped) => Some(SagaId::new(shipped.saga.clone())),
        }
    }

    async fn handle(
        &mut self,
        input: Self::Subscribe,
        ctx: &mut SagaContext,
    ) -> Result<SagaEffect<Self::Event>, Self::Error> {
        self.handled.lock().expect(FIXTURE_LOCK).push(Seen {
            source: ctx.upstream_aggregate_id().as_str().to_owned(),
            sequence: ctx.upstream_sequence(),
        });
        let note = match input {
            Upstream::Placed(placed) => placed.note,
            Upstream::Shipped(shipped) => shipped.note,
        };
        if note == REFUSE {
            return Err(Refused);
        }
        Ok(SagaEffect::None)
    }
}

// Wiring

/// The stream the source a test is not about is bound to. Nothing writes it, so
/// that source settles nothing and records nothing.
pub const QUIET_KEY: &str = "manager-offset-quiet-source";

/// Read one stream from its beginning, at the cadence every test here waits
/// against.
pub fn from_stream(key: &str) -> Subscription {
    Subscription::stream(key).with_poll_interval(TEST_POLL_INTERVAL)
}

/// The source a test is not about.
pub fn quiet() -> Subscription {
    from_stream(QUIET_KEY)
}

/// Host [`OffsetSaga`] on `system` as [`HOST_ID`], with `partitions`
/// partitions, its [`Placed`] source bound by `placed` and its [`Shipped`]
/// source by `shipped`.
pub async fn spawn_offset_saga(
    system: &DeliverySystem,
    placed: Subscription,
    shipped: Subscription,
    partition_count: usize,
    handled: &Handled,
    made: &Made,
) -> SagaHostProxy<OffsetSaga> {
    let handled = Arc::clone(handled);
    let made = Arc::clone(made);
    SagaManagerProps::<OffsetSaga>::new(move |_components| {
        made.fetch_add(1, Ordering::SeqCst);
        OffsetSaga::recording(&handled)
    })
    .observing::<Placed>(placed)
    .observing::<Shipped>(shipped)
    .with_delivery_id(host_id())
    .with_partitions(partitions(partition_count))
    .with_delivery_poll_interval(TEST_POLL_INTERVAL)
    .spawn(system)
    .await
}

/// Append one [`Placed`] at `sequence` of `stream`, addressed to `saga`.
pub async fn append_placed(
    journal: &dyn EventStore,
    stream: &str,
    sequence: u64,
    saga: &str,
    note: &str,
) -> u64 {
    append_row(
        journal,
        stream,
        sequence,
        &Placed {
            saga: saga.to_owned(),
            note: note.to_owned(),
        },
    )
    .await
}

/// Append one [`Shipped`] at `sequence` of `stream`, addressed to `saga`.
pub async fn append_shipped(
    journal: &dyn EventStore,
    stream: &str,
    sequence: u64,
    saga: &str,
    note: &str,
) -> u64 {
    append_row(
        journal,
        stream,
        sequence,
        &Shipped {
            saga: saga.to_owned(),
            note: note.to_owned(),
        },
    )
    .await
}

// Probes

pub fn seen(handled: &Handled) -> Vec<Seen> {
    handled.lock().expect(FIXTURE_LOCK).clone()
}

/// The sequences of `source` that reached `handle`, in arrival order.
pub fn sequences_of(handled: &Handled, source: &str) -> Vec<u64> {
    seen(handled)
        .into_iter()
        .filter(|s| s.source == source)
        .map(|s| s.sequence)
        .collect()
}

/// Wait until `select` holds of what reached `handle`, then answer all of it.
pub async fn wait_until_handled(
    handled: &Handled,
    select: impl Fn(&[Seen]) -> bool,
    label: &str,
) -> Vec<Seen> {
    let deadline = Instant::now() + PROBE_TIMEOUT;
    loop {
        let current = seen(handled);
        if select(&current) {
            return current;
        }
        assert!(Instant::now() < deadline, "{label}: handle saw {current:?}");
        tokio::time::sleep(PROBE_INTERVAL).await;
    }
}

/// Wait until `sequence` of `source` has reached `handle` at least `times`
/// times.
pub async fn wait_until_handled_times(
    handled: &Handled,
    source: &str,
    sequence: u64,
    times: usize,
    label: &str,
) -> Vec<Seen> {
    wait_until_handled(
        handled,
        |current| {
            current
                .iter()
                .filter(|s| s.source == source && s.sequence == sequence)
                .count()
                >= times
        },
        label,
    )
    .await
}

/// Stop the manager `host` fronts and wait until it has left the runtime's
/// registry.
///
/// A process is unregistered only after its children are, so the manager's pid
/// leaving the registry says every source partition it ran — and every
/// instance it fronted — is over: nothing of the stopped host can still read a
/// source or record a position while the test judges what it left behind.
pub async fn stop_and_wait(system: &DeliverySystem, host: &SagaHostProxy<OffsetSaga>) {
    host.manager().stop().await.expect("stop must succeed");
    wait_until_gone(system.process_system(), host.manager().pid()).await;
}

async fn wait_until_gone(ps: &ProcessSystem, pid: Pid) {
    let deadline = Instant::now() + PROBE_TIMEOUT;
    loop {
        if ps.lookup(pid).await.is_none() {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "the stopped manager must leave the registry"
        );
        tokio::time::sleep(PROBE_INTERVAL).await;
    }
}

// A journal that records the reads of a source and can refuse a saga's writes

/// An [`EventStore`] that records every query it is handed, can be switched to
/// refuse every append addressed to one stream, and can panic on a chosen load
/// of one source stream.
///
/// The refusal is a switch rather than a budget: what a test needs is a span
/// during which a saga's appends fail however often they are retried, and a
/// later span during which they land, on one journal throughout.
///
/// Its `Clone` is the handle's: the record, the switch and the counter are one
/// region behind every handle, so a fixture that names this journal in a
/// wiring and keeps a handle of its own reads what the wiring asked for and
/// flips what the wiring meets.
#[derive(Clone)]
pub struct SourceJournal {
    inner: InMemoryEventStore,
    calls: Arc<Mutex<Vec<LoadQuery>>>,
    refused: Option<String>,
    refusing: Arc<AtomicBool>,
    panic_on: Option<(String, usize)>,
    panic_stream_loads: Arc<AtomicUsize>,
}

impl SourceJournal {
    pub fn recording() -> Self {
        Self {
            inner: InMemoryEventStore::default(),
            calls: Arc::new(Mutex::new(Vec::new())),
            refused: None,
            refusing: Arc::new(AtomicBool::new(false)),
            panic_on: None,
            panic_stream_loads: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// A journal whose appends to `stream` are refused while [`refuse`](Self::refuse)
    /// is switched on. It starts switched off.
    pub fn refusing_appends_to(stream: &str) -> Self {
        Self {
            refused: Some(stream.to_owned()),
            ..Self::recording()
        }
    }

    /// A journal that panics on the `nth` (1-based) load naming `stream`, after
    /// recording it, so whatever reads that stream is restarted by supervision.
    pub fn panicking_on_load_of(stream: &str, nth: usize) -> Self {
        Self {
            panic_on: Some((stream.to_owned(), nth)),
            ..Self::recording()
        }
    }

    pub fn refuse(&self, on: bool) {
        self.refusing.store(on, Ordering::SeqCst);
    }

    /// Every query naming `stream`, in call order.
    ///
    /// The manager reads a stream-bound source by naming its key; an instance
    /// replays by naming its own id, and the delivery host names no key at all,
    /// so the queries naming a source's key are that source's reads.
    pub fn stream_queries(&self, stream: &str) -> Vec<LoadQuery> {
        self.calls
            .lock()
            .expect(FIXTURE_LOCK)
            .iter()
            .filter(|query| query.stream_key.as_deref() == Some(stream))
            .cloned()
            .collect()
    }

    /// The position every read of `stream` asked to start from, in call order.
    pub fn asked_from(&self, stream: &str) -> Vec<Option<u64>> {
        self.stream_queries(stream)
            .iter()
            .map(|query| query.from_stream_sequence)
            .collect()
    }

    /// Forget every query recorded so far, so what follows is read on its own.
    pub fn forget_queries(&self) {
        self.calls.lock().expect(FIXTURE_LOCK).clear();
    }
}

#[async_trait]
impl EventStore for SourceJournal {
    async fn append(
        &self,
        key: &str,
        events: Vec<AppendingEvent>,
    ) -> Result<AppendOutcome, AppendError> {
        if self.refusing.load(Ordering::SeqCst) && self.refused.as_deref() == Some(key) {
            return Err(AppendError::Backend(Box::new(std::io::Error::other(
                "the fixture journal refuses appends to this stream",
            ))));
        }
        self.inner.append(key, events).await
    }

    async fn load(&self, query: LoadQuery) -> Result<EventStream<'_>, LoadError> {
        self.calls.lock().expect(FIXTURE_LOCK).push(query.clone());
        let nth = match &self.panic_on {
            Some((stream, _)) if query.stream_key.as_deref() == Some(stream.as_str()) => {
                self.panic_stream_loads.fetch_add(1, Ordering::SeqCst) + 1
            }
            _ => 0,
        };
        if matches!(&self.panic_on, Some((_, panic_at)) if *panic_at == nth) {
            panic!("the fixture journal panics on load {nth} of the source stream");
        }
        self.inner.load(query).await
    }
}

/// Wait until at least `want` reads of `stream` have been made, then answer the
/// position each asked from.
pub async fn wait_until_reads(
    journal: &SourceJournal,
    stream: &str,
    want: usize,
    label: &str,
) -> Vec<Option<u64>> {
    let deadline = Instant::now() + PROBE_TIMEOUT;
    loop {
        let asked = journal.asked_from(stream);
        if asked.len() >= want {
            return asked;
        }
        assert!(
            Instant::now() < deadline,
            "{label}: {stream} was read {} times, wanted {want}; asked from {asked:?}",
            asked.len()
        );
        tokio::time::sleep(PROBE_INTERVAL).await;
    }
}
