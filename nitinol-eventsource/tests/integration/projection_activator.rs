//! A system's projection activator wakes an idle projector when something it
//! follows is appended, so a projector can be left to passivate.
//!
//! ```gherkin
//!   Scenario: an idle projector stops and an append notification wakes it
//!     Given a projector with `with_idle_timeout(After(d))` that has caught up
//!     When the journal stays quiet for longer than d
//!     Then the projector is stopped (its handle reports it inactive)
//!     When the same system's aggregate appends an event the projector handles
//!     Then the projector is activated again without any watcher, projects the
//!          event, and its offset moves on
//!
//!   Scenario: a running projector projects without waiting out its poll interval
//!     Given a running projector with a 10 s poll interval
//!     When the same system's aggregate appends an event it follows
//!     Then the projector projects it well inside the 10 s
//!
//!   Scenario: an unrelated event does not wake a passivated projector
//!     Given a passivated projector, and a watcher that has had a turn since
//!           the event was appended
//!     When an event matching neither its origin nor its handlers is appended
//!     Then the projector stays stopped
//!
//!   Scenario: the watcher picks up a write that no notification announced
//!     Given a passivated projector
//!     When an event it follows is appended through a journal with no observer
//!     And the watcher interval passes
//!     Then the projector is activated again and projects the event
//!
//!   Scenario: an event learned of just before the idle stop is not dropped
//!     Given a running projector polling every 10 s that has settled once, and
//!           an event the activator learns of that the projector refuses once
//!     When the idle timeout stops the projector with that event unsettled
//!     Then the activator activates it again at once, not a poll interval
//!          later, and the event is projected
//!
//!   Scenario: under the system default idle timeout a projector stays reachable
//!     Given a system with a short default idle timeout and a projector naming
//!           no idle timeout of its own
//!     When the journal stays quiet for longer than the default, and an event
//!          is appended afterwards
//!     Then the projector projects it
//!
//!   Scenario: a second spawn of the same projection joins the running one
//!     Given a running projector
//!     When the same projection id is spawned again
//!     Then no second projector is started and the first declaration is kept
//!
//!   Scenario: stopping through the handle takes the projector out
//!     Given a running projector's handle
//!     When it is stopped
//!     Then the projector is inactive, and a later event it follows does not
//!          activate it again
//!
//!   Scenario: the watcher's interval is configurable and it can be disabled
//!     Given a passivated projector and a write through a journal with no
//!           observer
//!     Then a disabled watcher never picks it up, and the default watcher waits
//!          an order of magnitude longer than a projector's poll
//!
//!   Scenario: an activation that fails is tried again without another append
//!     Given a passivated projector, and an offset store whose loads fail
//!     When an event it follows is appended
//!     And the store recovers, with nothing further appended
//!     Then the projector is activated again and projects the event
//!
//!   Scenario: an event a first read could not settle is not left behind
//!     Given an event journaled before its projector was spawned, whose first
//!           projection fails, and an idle timeout shorter than the poll interval
//!     When the idle timeout stops the projector before its next read
//!     Then the activator activates it again, and the event is projected
//!
//!   Scenario: a first read that fails is read again after an idle stop
//!     Given an event journaled before its projector was spawned, a journal
//!           whose reads fail, and an idle timeout shorter than the poll interval
//!     When the idle timeout stops the projector before any read succeeded
//!     And the journal recovers, with nothing further appended
//!     Then the activator activates it again, and the event is projected
//!
//!   Scenario: an at-most-once offset save that failed does not repeat an event
//!     Given an at-most-once projector with an idle timeout, and an offset store
//!           whose saves fail
//!     When an event it follows is appended, and the projector stops idle and
//!          is activated again
//!     And the store recovers, and a second event is appended
//!     Then each event has been handled exactly once
//!
//!   Scenario: history from before the activator started does not wake a
//!             projector whose position already covers it
//!     Given a journal written before the activator started, and a recorded
//!           projector position covering its records
//!     When the projector is spawned, passivates, and the watcher has had turns
//!     Then the projector is not activated again — the watcher reads from the
//!          journal's head as the activator started, not from its beginning
//!
//!   Scenario: a poller restarted by its supervision starts no second projector
//!     Given a projector whose poller's first read panics
//!     When the poller's supervision restarts it and it reads again
//!     Then the one projector projects the event once, and no second
//!          activation starts
//!
//!   Scenario: a read that failed does not hold back the next append's wake
//!     Given a running projector polling every 10 s, and a journal whose reads
//!           fail
//!     When an append is announced and its read fails, the journal recovers,
//!          and another is appended
//!     Then the projector projects both well inside the 10 s
//!
//!   Scenario: a re-registered projection's restored offset is not owed again
//!     Given a projection stopped through its handle after projecting an event,
//!           and spawned again before the watcher has read that event
//!     When the watcher reads it
//!     Then no further activation follows
//!
//!   Scenario: an offset that could not be loaded is loaded again after an
//!             idle stop, though the journal is empty
//!     Given an empty journal, an offset store whose loads fail, and an idle
//!           timeout shorter than the poll interval
//!     When the offset store recovers, with nothing appended
//!     Then the activator activates the projector again and it starts
//!
//!   Scenario: a watcher whose head could not be read passes over no write
//!     Given a journal whose head cannot be read as the activator starts
//!     When a write no notification announced is made after the projector
//!          passivated
//!     Then the watcher picks it up and the projector projects it
//!
//!   Scenario: a stream projector weighs what it is owed in its stream's units
//!     Given a projector following one stream, written interleaved with another
//!     When it settles that stream's event and passivates
//!     Then it stays passivated, though the event's global position is past its
//!          stream offset
//!     When its stream is appended to again
//!     Then it is activated again and projects the event
//! ```
//!
//! How many activations a projection has had is counted where each one starts:
//! every activation builds its projector from the producer once, and nothing
//! else calls it. Each test holds its own counter, so nothing leaks between
//! tests running in one binary.
//!
//! What reaches a projector is told apart by path. Appends through
//! `EventSourceSystem::event_store()` — and through the aggregates the system
//! spawns — are the ones the activator is notified of; appends through
//! `PersistenceProvider::event_store()` carry no observer, so only the watcher
//! can learn of them. Where a test is about the notification, the watcher is
//! disabled, so nothing else can be what woke the projector. The watcher keeps
//! no position of its own, so where a test needs it to have read past an
//! event, it waits out two watcher intervals after the event is in the journal.
//!
//! The tests run on paused time: idle timers, quiet periods, poll and watcher
//! intervals and the probe bounds are all virtual, so a quiet period is exactly
//! as long as it is written.
//!
//! Run with:
//! `cargo test -p nitinol-eventsource --test integration projection_activator::`.

use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use tokio::sync::Notify;
use tokio::time::Instant;

use nitinol_eventsource::codec::Codec;
use nitinol_eventsource::system::EventSourceSystem;
use nitinol_eventsource::{
    Aggregate, Decider, Decision, Event, IdleTimeout, Never, ProjectionContext, ProjectionId,
    Projector, ProjectorProps, DEFAULT_POLL_INTERVAL,
};
use nitinol_persistence::error::{AppendError, LoadError, OffsetError};
use nitinol_persistence::provider::{
    CustomStore, PersistenceProvider, ProjectionOffsetSlot, Store, Unprovided,
};
use nitinol_persistence::store::{
    DeliveryMode, EventStore, EventStream, InMemoryEventStore, InMemoryOffsetStore, InMemoryStore,
    Journal, OffsetScope, OffsetStore,
};
use nitinol_persistence::{
    AggregateId, AppendOutcome, AppendingEvent, EventType, Family, LoadQuery, TypeName,
};
use nitinol_runtime::ProcessSystem;

/// The idle timeout a projector under test stops after.
const IDLE: Duration = Duration::from_millis(50);

/// A quiet period long enough that a projector subject to [`IDLE`] has stopped
/// by its end.
const QUIET: Duration = Duration::from_millis(200);

/// A watcher interval short enough to be waited out inside [`PROBE_TIMEOUT`],
/// and far enough below any default an order of magnitude above
/// [`DEFAULT_POLL_INTERVAL`] that a watcher ignoring it cannot pass.
const WATCHER_INTERVAL: Duration = Duration::from_secs(1);

/// A poll interval far longer than anything a nudged read may take.
const LONG_POLL: Duration = Duration::from_secs(10);

/// How soon a running projector must project an append it was nudged for.
const NUDGED_WITHIN: Duration = Duration::from_secs(1);

/// How long a disabled or default watcher is given to show itself.
const WATCHER_HORIZON: Duration = Duration::from_secs(60);

/// How long, in virtual time, any single wait below may take before it is a
/// failure.
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// How often a wait below looks again.
const PROBE_STEP: Duration = Duration::from_millis(5);

// Fixtures: the events

/// Pass-through codec: neither event carries a payload, so the format only has
/// to exist.
struct UnitCodec;

impl Codec<Watched> for UnitCodec {
    type Error = std::convert::Infallible;

    fn encode(_event: &Watched) -> Result<Bytes, Self::Error> {
        Ok(Bytes::new())
    }

    fn decode(_payload: &[u8]) -> Result<Watched, Self::Error> {
        Ok(Watched)
    }
}

impl Codec<Unwatched> for UnitCodec {
    type Error = std::convert::Infallible;

    fn encode(_event: &Unwatched) -> Result<Bytes, Self::Error> {
        Ok(Bytes::new())
    }

    fn decode(_payload: &[u8]) -> Result<Unwatched, Self::Error> {
        Ok(Unwatched)
    }
}

/// The event every projector here handles.
#[derive(Clone, Debug, PartialEq)]
struct Watched;

impl Event for Watched {
    const EVENT_TYPE: EventType = EventType::new(
        Family::new("projection.activator"),
        TypeName::new("Watched"),
    );

    type Codec = UnitCodec;
}

/// An event no projector here handles.
#[derive(Clone, Debug, PartialEq)]
struct Unwatched;

impl Event for Unwatched {
    const EVENT_TYPE: EventType = EventType::new(
        Family::new("projection.activator"),
        TypeName::new("Unwatched"),
    );

    type Codec = UnitCodec;
}

// Fixtures: the aggregate that appends through the system

#[derive(Default)]
struct Tally {
    value: u64,
}

impl Aggregate for Tally {
    type Event = Watched;
    type Recover = Never;

    fn apply(&mut self, _event: Watched) {
        self.value += 1;
    }
}

/// Persists one [`Watched`].
struct Record;

impl Decider<Record> for Tally {
    type Output = ();
    type Rejection = std::convert::Infallible;

    fn decide(&self, _cmd: Record) -> Decision<Watched, (), Self::Rejection> {
        Decision::persist(vec![Watched]).output(())
    }
}

// Fixtures: the projection

/// What one projection's activations have applied, and how many there were.
#[derive(Clone, Default)]
struct Projection {
    applied: Arc<AtomicU64>,
    reached: Arc<Notify>,
    activations: Arc<AtomicUsize>,
    /// How many more records are refused before one is applied.
    refusals: Arc<AtomicUsize>,
}

impl Projection {
    /// The producer a projector is declared with: each call is one activation.
    fn producer(&self) -> impl Fn() -> CountingProjector + Send + Sync + 'static {
        let projection = self.clone();
        move || {
            projection.activations.fetch_add(1, Ordering::SeqCst);
            CountingProjector {
                projection: projection.clone(),
            }
        }
    }

    /// Have the next `count` records projected be refused.
    fn refuse_next(&self, count: usize) {
        self.refusals.store(count, Ordering::SeqCst);
    }

    fn applied(&self) -> u64 {
        self.applied.load(Ordering::SeqCst)
    }

    fn activations(&self) -> usize {
        self.activations.load(Ordering::SeqCst)
    }

    /// Wait until `expected` records have been applied.
    async fn counted(&self, expected: u64, why: &str) {
        let watch = async {
            loop {
                let notified = self.reached.notified();
                if self.applied() >= expected {
                    return;
                }
                notified.await;
            }
        };
        tokio::time::timeout(PROBE_TIMEOUT, watch)
            .await
            .unwrap_or_else(|_| {
                panic!(
                    "{why}: {} records applied in {PROBE_TIMEOUT:?}, wanted {expected}",
                    self.applied()
                )
            });
    }
}

struct CountingProjector {
    projection: Projection,
}

#[derive(Debug, thiserror::Error)]
#[error("the projection refused the record")]
struct Refused;

#[async_trait]
impl Projector<Watched> for CountingProjector {
    type Error = Refused;

    async fn project(
        &mut self,
        _event: Watched,
        _ctx: &mut ProjectionContext<'_, ()>,
    ) -> Result<(), Self::Error> {
        let refused = self
            .projection
            .refusals
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |left| {
                left.checked_sub(1)
            })
            .is_ok();
        if refused {
            return Err(Refused);
        }
        self.projection.applied.fetch_add(1, Ordering::SeqCst);
        self.projection.reached.notify_one();
        Ok(())
    }
}

// Fixtures: a journal whose next read can be made to panic

/// An in-memory journal whose next `load`, once armed, panics instead of
/// reading, and counts the loads it panicked in; appends are never touched.
///
/// A panic is what a poller's supervision answers with a restart: the poller
/// turns a panicking read into a handler error, and it runs under `Restart`.
#[derive(Clone, Default)]
struct PanickyJournal {
    inner: InMemoryEventStore,
    armed: Arc<AtomicBool>,
    panicked: Arc<AtomicUsize>,
}

impl PanickyJournal {
    fn panic_on_next_load(&self) {
        self.armed.store(true, Ordering::SeqCst);
    }

    fn panicked(&self) -> usize {
        self.panicked.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl EventStore for PanickyJournal {
    async fn append(
        &self,
        key: &str,
        events: Vec<AppendingEvent>,
    ) -> Result<AppendOutcome, AppendError> {
        self.inner.append(key, events).await
    }

    async fn load(&self, query: LoadQuery) -> Result<EventStream<'_>, LoadError> {
        if self.armed.swap(false, Ordering::SeqCst) {
            self.panicked.fetch_add(1, Ordering::SeqCst);
            panic!("intentional journal load panic");
        }
        self.inner.load(query).await
    }

    async fn head(&self) -> Result<Option<u64>, LoadError> {
        self.inner.head().await
    }
}

type PanickyJournalStore = CustomStore<Unprovided, InMemoryOffsetStore, Unprovided>;

// Fixtures: an offset store whose loads can be made to fail

/// An in-memory offset store whose `load` fails while its switch is set, and
/// counts the loads it refused; `save` has a switch and a count of its own.
#[derive(Clone, Default)]
struct FlakyOffsets {
    inner: InMemoryOffsetStore,
    failing: Arc<AtomicBool>,
    refused: Arc<AtomicUsize>,
    failing_saves: Arc<AtomicBool>,
    refused_saves: Arc<AtomicUsize>,
}

impl FlakyOffsets {
    fn fail_loads(&self, failing: bool) {
        self.failing.store(failing, Ordering::SeqCst);
    }

    fn refused(&self) -> usize {
        self.refused.load(Ordering::SeqCst)
    }

    fn fail_saves(&self, failing: bool) {
        self.failing_saves.store(failing, Ordering::SeqCst);
    }

    fn refused_saves(&self) -> usize {
        self.refused_saves.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl OffsetStore for FlakyOffsets {
    type Tx = ();

    async fn load(&self, scope: &OffsetScope) -> Result<Option<u64>, OffsetError> {
        if self.failing.load(Ordering::SeqCst) {
            self.refused.fetch_add(1, Ordering::SeqCst);
            return Err(OffsetError::Backend(Box::new(std::io::Error::other(
                "intentional offset load failure",
            ))));
        }
        self.inner.load(scope).await
    }

    async fn save(
        &self,
        scope: &OffsetScope,
        sequence: u64,
        tx: Option<&mut Self::Tx>,
    ) -> Result<(), OffsetError> {
        if self.failing_saves.load(Ordering::SeqCst) {
            self.refused_saves.fetch_add(1, Ordering::SeqCst);
            return Err(OffsetError::Backend(Box::new(std::io::Error::other(
                "intentional offset save failure",
            ))));
        }
        self.inner.save(scope, sequence, tx).await
    }
}

type FlakyStore = CustomStore<Unprovided, FlakyOffsets, Unprovided>;

// Fixtures: a journal whose reads can be made to fail

/// An in-memory journal whose `load` fails while its switch is set, and counts
/// the loads it refused; appends are never refused.
#[derive(Clone, Default)]
struct FlakyJournal {
    inner: InMemoryEventStore,
    failing: Arc<AtomicBool>,
    refused: Arc<AtomicUsize>,
}

impl FlakyJournal {
    fn fail_loads(&self, failing: bool) {
        self.failing.store(failing, Ordering::SeqCst);
    }

    fn refused(&self) -> usize {
        self.refused.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl EventStore for FlakyJournal {
    async fn append(
        &self,
        key: &str,
        events: Vec<AppendingEvent>,
    ) -> Result<AppendOutcome, AppendError> {
        self.inner.append(key, events).await
    }

    async fn load(&self, query: LoadQuery) -> Result<EventStream<'_>, LoadError> {
        if self.failing.load(Ordering::SeqCst) {
            self.refused.fetch_add(1, Ordering::SeqCst);
            return Err(LoadError::Backend(Box::new(std::io::Error::other(
                "intentional journal load failure",
            ))));
        }
        self.inner.load(query).await
    }

    async fn head(&self) -> Result<Option<u64>, LoadError> {
        self.inner.head().await
    }
}

type FlakyJournalStore = CustomStore<Unprovided, InMemoryOffsetStore, Unprovided>;

// Fixtures: a journal whose head cannot be read at first

/// An in-memory journal whose `head` fails the first time it is asked.
#[derive(Clone, Default)]
struct HeadlessAtFirst {
    inner: InMemoryEventStore,
    asked: Arc<AtomicBool>,
}

#[async_trait]
impl EventStore for HeadlessAtFirst {
    async fn append(
        &self,
        key: &str,
        events: Vec<AppendingEvent>,
    ) -> Result<AppendOutcome, AppendError> {
        self.inner.append(key, events).await
    }

    async fn load(&self, query: LoadQuery) -> Result<EventStream<'_>, LoadError> {
        self.inner.load(query).await
    }

    async fn head(&self) -> Result<Option<u64>, LoadError> {
        if self.asked.swap(true, Ordering::SeqCst) {
            return self.inner.head().await;
        }
        Err(LoadError::Backend(Box::new(std::io::Error::other(
            "intentional journal head failure",
        ))))
    }
}

type HeadlessAtFirstStore = CustomStore<Unprovided, InMemoryOffsetStore, Unprovided>;

// Helpers: systems

/// Whether and how often the system's watcher runs.
enum Watcher {
    Disabled,
    Every(Duration),
    Default,
}

async fn without_default_idle() -> ProcessSystem {
    ProcessSystem::new().await
}

async fn with_default_idle() -> ProcessSystem {
    ProcessSystem::builder()
        .with_default_idle_timeout(IDLE)
        .build()
        .await
}

fn system_on(process_system: ProcessSystem, watcher: Watcher) -> EventSourceSystem<InMemoryStore> {
    let builder = EventSourceSystem::builder(process_system)
        .with_persistence(PersistenceProvider::new(InMemoryStore::default()));
    match watcher {
        Watcher::Disabled => builder.without_projection_watcher().build(),
        Watcher::Every(interval) => builder.with_projection_watcher_interval(interval).build(),
        Watcher::Default => builder.build(),
    }
}

fn flaky_system(
    process_system: ProcessSystem,
    offsets: &FlakyOffsets,
) -> EventSourceSystem<FlakyStore> {
    EventSourceSystem::builder(process_system)
        .with_persistence(PersistenceProvider::new(
            CustomStore::builder()
                .eventstore(InMemoryEventStore::default())
                .projection_offsets(offsets.clone())
                .build(),
        ))
        .without_projection_watcher()
        .build()
}

fn flaky_journal_system(
    process_system: ProcessSystem,
    journal: &FlakyJournal,
) -> EventSourceSystem<FlakyJournalStore> {
    EventSourceSystem::builder(process_system)
        .with_persistence(PersistenceProvider::new(
            CustomStore::builder()
                .eventstore(journal.clone())
                .projection_offsets(InMemoryOffsetStore::default())
                .build(),
        ))
        .without_projection_watcher()
        .build()
}

fn panicky_journal_system(
    process_system: ProcessSystem,
    journal: &PanickyJournal,
) -> EventSourceSystem<PanickyJournalStore> {
    EventSourceSystem::builder(process_system)
        .with_persistence(PersistenceProvider::new(
            CustomStore::builder()
                .eventstore(journal.clone())
                .projection_offsets(InMemoryOffsetStore::default())
                .build(),
        ))
        .without_projection_watcher()
        .build()
}

// Helpers: appends and positions

/// Append one event of `event_type` at `sequence` of `stream`, answering the
/// global position it was given.
async fn append(
    journal: &Journal,
    stream: &AggregateId,
    sequence: u64,
    event_type: EventType,
) -> u64 {
    let outcome = journal
        .append(
            stream.as_str(),
            vec![AppendingEvent {
                sequence,
                event_type,
                payload: Bytes::new(),
                occurred_at: jiff::Timestamp::now(),
            }],
        )
        .await
        .expect("appending the next sequence of a stream must succeed");
    *outcome
        .assigned_sequences
        .last()
        .expect("one appended event is given one global position")
}

/// The position `scope` is recorded at in the system's projection slot.
async fn recorded<St>(system: &EventSourceSystem<St>, scope: &OffsetScope) -> Option<u64>
where
    St: ProjectionOffsetSlot,
{
    system
        .persistence()
        .projection_offsets()
        .load(scope)
        .await
        .expect("loading from the in-memory offset store must succeed")
}

// Helpers: waits

/// Wait until `holds` answers true, failing the test once `within` has passed.
async fn until_within<F, Fut>(what: &str, within: Duration, mut holds: F)
where
    F: FnMut() -> Fut,
    Fut: Future<Output = bool>,
{
    let deadline = Instant::now() + within;
    while !holds().await {
        assert!(
            Instant::now() < deadline,
            "{what} did not hold within {within:?}"
        );
        tokio::time::sleep(PROBE_STEP).await;
    }
}

async fn until<F, Fut>(what: &str, holds: F)
where
    F: FnMut() -> Fut,
    Fut: Future<Output = bool>,
{
    until_within(what, PROBE_TIMEOUT, holds).await;
}

/// Wait until the system's watcher, running every [`WATCHER_INTERVAL`], has had
/// a read since global position `position` was in the journal.
///
/// The watcher keeps no position to read back, so this is a wait of two of its
/// intervals: whichever way its ticks fall, one read starts and ends within
/// them, on virtual time that passes exactly as written.
async fn watcher_has_had_a_read_past<St>(system: &EventSourceSystem<St>, position: u64)
where
    St: Store,
{
    let head = system
        .event_store()
        .head()
        .await
        .expect("reading the head of the in-memory journal must succeed");
    assert!(
        head.is_some_and(|head| head >= position),
        "global position {position} must be in the journal the watcher reads before it is \
         waited on, but the head is {head:?}"
    );
    tokio::time::sleep(WATCHER_INTERVAL * 2).await;
}

// Scenario: an idle projector stops and an append notification wakes it

/// The watcher is disabled, so the only thing that can bring the projector
/// back is the notification of the aggregate's append — which reaches the
/// activator only if the aggregate appends through the system's observed
/// journal.
#[tokio::test(start_paused = true)]
async fn an_idle_projector_stops_and_the_systems_aggregate_append_wakes_it() {
    // Given: a projector that has caught up with the aggregate's first event
    let system = system_on(without_default_idle().await, Watcher::Disabled);
    let id = ProjectionId::new("projection-activator-wake");
    let stream = AggregateId::new("projection-activator-wake");
    let projection = Projection::default();
    let handle = ProjectorProps::new(id.clone(), projection.producer())
        .with_idle_timeout(IdleTimeout::After(IDLE))
        .with_event::<Watched>()
        .catchup_from_aggregate(stream.clone())
        .spawn(&system)
        .await;
    let aggregate = system.spawn_aggregate::<Tally>(stream.clone()).await;
    aggregate
        .ask(Record)
        .await
        .expect("the aggregate must persist its first event");
    projection
        .counted(1, "the projector must catch up with the first event")
        .await;

    // When: the journal stays quiet for longer than the idle timeout
    tokio::time::sleep(QUIET).await;

    // Then: the projector has stopped and is out of the registry
    let handle_ref = &handle;
    until(
        "the projector stopping on its idle timeout",
        move || async move { !handle_ref.is_active().await },
    )
    .await;
    assert_eq!(
        projection.activations(),
        1,
        "a projector stopped on its idle timeout must stay stopped while nothing it \
         follows is appended"
    );

    // When: the same system's aggregate appends an event the projector handles
    aggregate
        .ask(Record)
        .await
        .expect("the aggregate must persist its second event");

    // Then
    projection
        .counted(
            2,
            "the append notification must wake the passivated projector without any watcher",
        )
        .await;
    assert_eq!(
        projection.activations(),
        2,
        "the second event must be projected by a second activation, built anew from the \
         producer"
    );
    assert_eq!(
        recorded(&system, &OffsetScope::from(&id)).await,
        Some(2),
        "the re-activated projector must carry its offset on to the second event"
    );
}

// Scenario: a running projector projects without waiting out its poll interval

#[tokio::test(start_paused = true)]
async fn a_running_projector_projects_an_append_without_waiting_out_its_poll_interval() {
    // Given: a projector polling every 10 s that has caught up
    let system = system_on(without_default_idle().await, Watcher::Disabled);
    let stream = AggregateId::new("projection-activator-nudge");
    let aggregate = system.spawn_aggregate::<Tally>(stream.clone()).await;
    aggregate
        .ask(Record)
        .await
        .expect("the aggregate must persist its first event");
    let projection = Projection::default();
    let _handle = ProjectorProps::new(
        ProjectionId::new("projection-activator-nudge"),
        projection.producer(),
    )
    .with_event::<Watched>()
    .catchup_from_aggregate(stream.clone())
    .with_poll_interval(LONG_POLL)
    .spawn(&system)
    .await;
    projection
        .counted(1, "the first read must catch up without waiting")
        .await;

    // When
    let appended_at = Instant::now();
    aggregate
        .ask(Record)
        .await
        .expect("the aggregate must persist its second event");
    projection
        .counted(2, "the running projector must project the appended event")
        .await;
    let projected_in = appended_at.elapsed();

    // Then
    assert!(
        projected_in < NUDGED_WITHIN,
        "the appended event took {projected_in:?} to be projected by a projector polling \
         every {LONG_POLL:?}; the append notification must nudge its poller to read at once"
    );
}

// Scenario: an unrelated event does not wake a passivated projector

/// A projector following every stream is matched by event type alone, so an
/// event of a type it does not handle matches neither its origin's key nor its
/// handlers. Both paths that could report it run: the notification, and the
/// watcher, which is waited on until it has read past the event.
#[tokio::test(start_paused = true)]
async fn an_event_type_a_global_projector_does_not_handle_leaves_it_stopped() {
    // Given: a passivated projector following every stream
    let system = system_on(
        without_default_idle().await,
        Watcher::Every(WATCHER_INTERVAL),
    );
    let journal = system.event_store();
    let projection = Projection::default();
    let handle = ProjectorProps::new(
        ProjectionId::new("projection-activator-unrelated-type"),
        projection.producer(),
    )
    .with_event::<Watched>()
    .catchup_from_global()
    .with_idle_timeout(IdleTimeout::After(IDLE))
    .spawn(&system)
    .await;
    append(
        &journal,
        &AggregateId::new("projection-activator-unrelated-type-handled"),
        1,
        Watched::EVENT_TYPE,
    )
    .await;
    projection
        .counted(1, "the projector must catch up with the handled event")
        .await;
    tokio::time::sleep(QUIET).await;
    let handle_ref = &handle;
    until(
        "the projector stopping on its idle timeout",
        move || async move { !handle_ref.is_active().await },
    )
    .await;

    // When: an event of a type it does not handle is appended, and both the
    // notification and the watcher have had it
    let head = append(
        &journal,
        &AggregateId::new("projection-activator-unrelated-type-other"),
        1,
        Unwatched::EVENT_TYPE,
    )
    .await;
    watcher_has_had_a_read_past(&system, head).await;
    tokio::time::sleep(QUIET).await;

    // Then
    assert!(
        !handle.is_active().await,
        "an event of a type the projector does not handle must not activate it"
    );
    assert_eq!(
        projection.activations(),
        1,
        "nothing the projector follows was appended, so it must not have been activated again"
    );
}

/// A projector following one stream is matched by that stream's key, so an
/// event of a type it does handle, in another stream, matches neither.
#[tokio::test(start_paused = true)]
async fn an_event_in_another_stream_leaves_a_stream_projector_stopped() {
    // Given: a passivated projector following one stream
    let system = system_on(
        without_default_idle().await,
        Watcher::Every(WATCHER_INTERVAL),
    );
    let journal = system.event_store();
    let stream = AggregateId::new("projection-activator-unrelated-stream");
    let projection = Projection::default();
    let handle = ProjectorProps::new(
        ProjectionId::new("projection-activator-unrelated-stream"),
        projection.producer(),
    )
    .with_event::<Watched>()
    .with_idle_timeout(IdleTimeout::After(IDLE))
    .catchup_from_aggregate(stream.clone())
    .spawn(&system)
    .await;
    append(&journal, &stream, 1, Watched::EVENT_TYPE).await;
    projection
        .counted(1, "the projector must catch up with its stream's event")
        .await;
    tokio::time::sleep(QUIET).await;
    let handle_ref = &handle;
    until(
        "the projector stopping on its idle timeout",
        move || async move { !handle_ref.is_active().await },
    )
    .await;

    // When: an event of the handled type is appended to another stream
    let head = append(
        &journal,
        &AggregateId::new("projection-activator-unrelated-stream-other"),
        1,
        Watched::EVENT_TYPE,
    )
    .await;
    watcher_has_had_a_read_past(&system, head).await;
    tokio::time::sleep(QUIET).await;

    // Then
    assert!(
        !handle.is_active().await,
        "an event in a stream the projector does not follow must not activate it"
    );
    assert_eq!(
        projection.activations(),
        1,
        "nothing the projector follows was appended, so it must not have been activated again"
    );
}

// Scenario: the watcher picks up a write that no notification announced

/// The write goes through the provider's own journal, which carries no
/// observer — a writer in another process looks the same. The probe bound is
/// shorter than any default an order of magnitude above the poll interval, so
/// it is the configured interval that brings the projector back.
#[tokio::test(start_paused = true)]
async fn a_write_no_notification_announced_wakes_the_projector_after_the_watcher_interval() {
    // Given: a passivated projector
    let system = system_on(
        without_default_idle().await,
        Watcher::Every(WATCHER_INTERVAL),
    );
    let stream = AggregateId::new("projection-activator-watcher");
    let projection = Projection::default();
    let handle = ProjectorProps::new(
        ProjectionId::new("projection-activator-watcher"),
        projection.producer(),
    )
    .with_event::<Watched>()
    .catchup_from_aggregate(stream.clone())
    .with_idle_timeout(IdleTimeout::After(IDLE))
    .spawn(&system)
    .await;
    append(&system.event_store(), &stream, 1, Watched::EVENT_TYPE).await;
    projection
        .counted(1, "the projector must catch up with the first event")
        .await;
    tokio::time::sleep(QUIET).await;
    let handle_ref = &handle;
    until(
        "the projector stopping on its idle timeout",
        move || async move { !handle_ref.is_active().await },
    )
    .await;

    // When: an event it follows is appended through a journal with no observer
    append(
        &system.persistence().event_store(),
        &stream,
        2,
        Watched::EVENT_TYPE,
    )
    .await;

    // Then
    projection
        .counted(
            2,
            "the watcher, at the interval the system was built with, must learn of the \
             unannounced write and wake the projector",
        )
        .await;
    assert_eq!(
        projection.activations(),
        2,
        "the unannounced write must be projected by a second activation"
    );
}

// Scenario: an event learned of just before the idle stop is not dropped

/// The projector polls every [`LONG_POLL`] and has settled one batch in this
/// activation, so the idle stop is not one the activator retries a poll
/// interval later. The second event is announced to the activator (R moves
/// past M) and read at once, but the projector refuses it, so nothing is
/// settled and its poller would read it again only a [`LONG_POLL`] later — far
/// past the idle timeout. What brings the event back well inside that is the
/// activator weighing R against M as the projector stops on its idle timeout.
#[tokio::test(start_paused = true)]
async fn an_event_learned_of_before_the_idle_stop_activates_the_projector_again_at_once() {
    // Given: a projector polling every 10 s that has settled the first event
    let system = system_on(without_default_idle().await, Watcher::Disabled);
    let journal = system.event_store();
    let id = ProjectionId::new("projection-activator-stop-race");
    let stream = AggregateId::new("projection-activator-stop-race");
    let projection = Projection::default();
    let _handle = ProjectorProps::new(id.clone(), projection.producer())
        .with_event::<Watched>()
        .catchup_from_global()
        .with_poll_interval(LONG_POLL)
        .with_idle_timeout(IdleTimeout::After(IDLE))
        .spawn(&system)
        .await;
    let first = append(&journal, &stream, 1, Watched::EVENT_TYPE).await;
    projection
        .counted(1, "the projector must catch up with the first event")
        .await;
    let (system_ref, scope) = (&system, &OffsetScope::from(&id));
    until(
        "the projector settling the first event",
        move || async move { recorded(system_ref, scope).await == Some(first) },
    )
    .await;
    assert!(IDLE < LONG_POLL);

    // When: an event the activator learns of is appended while the projector
    // still runs, the projector refuses it, and the idle timeout then stops it
    // with that event unsettled
    projection.refuse_next(1);
    let appended_at = Instant::now();
    let second = append(&journal, &stream, 2, Watched::EVENT_TYPE).await;

    // Then: it is projected by the next activation, not a poll interval later
    let projection_ref = &projection;
    until_within(
        "the event refused before the idle stop being projected",
        LONG_POLL * 2,
        move || async move { projection_ref.applied() >= 2 },
    )
    .await;
    let projected_in = appended_at.elapsed();
    assert!(
        projected_in < NUDGED_WITHIN,
        "the event the activator knew of took {projected_in:?} to be projected; a projector \
         that stops on its idle timeout owing it must be activated again at once, not after \
         its {LONG_POLL:?} poll interval"
    );
    assert_eq!(
        (projection.activations(), projection.applied()),
        (2, 2),
        "the refused event must be projected by the one activation that followed the idle \
         stop, and each event once — (activations, applied)"
    );
    until(
        "the re-activated projector settling the second event",
        move || async move { recorded(system_ref, scope).await == Some(second) },
    )
    .await;
}

// Scenario: under the system default idle timeout a projector stays reachable

/// Props that name no idle timeout inherit the system default: the projector
/// stops on it, and the append notification brings it back. The aggregate is
/// kept persistent so that only the projector passivates. The activator itself
/// runs under the same default, so it has to be persistent for the
/// notification to land anywhere.
#[tokio::test(start_paused = true)]
async fn a_projector_naming_no_idle_timeout_stops_on_the_system_default_and_is_woken() {
    // Given: a projector from the system's props that names no idle timeout
    let system = system_on(with_default_idle().await, Watcher::Disabled);
    let stream = AggregateId::new("projection-activator-inherit");
    let aggregate = system
        .aggregate_props::<Tally>(stream.clone())
        .with_idle_timeout(IdleTimeout::Persistent)
        .spawn()
        .await;
    let projection = Projection::default();
    let handle = system
        .projector_props(
            ProjectionId::new("projection-activator-inherit"),
            projection.producer(),
        )
        .with_event::<Watched>()
        .catchup_from_aggregate(stream.clone())
        .spawn()
        .await;
    aggregate
        .ask(Record)
        .await
        .expect("the aggregate must persist its first event");
    projection
        .counted(1, "the projector must catch up with the first event")
        .await;

    // When: the journal stays quiet for longer than the default
    tokio::time::sleep(QUIET).await;
    let handle_ref = &handle;
    until(
        "the projector stopping on the inherited system default",
        move || async move { !handle_ref.is_active().await },
    )
    .await;
    aggregate
        .ask(Record)
        .await
        .expect("the persistent aggregate must persist its second event");

    // Then
    projection
        .counted(
            2,
            "a projector under the system default idle timeout must project what is appended \
             after a quiet journal",
        )
        .await;
    assert_eq!(
        projection.activations(),
        2,
        "the projector must have stopped on the inherited default and been activated again"
    );
}

// Scenario: a second spawn of the same projection joins the running one

#[tokio::test(start_paused = true)]
async fn a_second_spawn_of_a_running_projection_starts_no_second_projector() {
    // Given: a running projector
    let system = system_on(without_default_idle().await, Watcher::Disabled);
    let id = ProjectionId::new("projection-activator-join");
    let stream = AggregateId::new("projection-activator-join");
    let first = Projection::default();
    let _first_handle = ProjectorProps::new(id.clone(), first.producer())
        .with_event::<Watched>()
        .catchup_from_aggregate(stream.clone())
        .spawn(&system)
        .await;

    // When: the same projection id is spawned again
    let second = Projection::default();
    let second_handle = ProjectorProps::new(id.clone(), second.producer())
        .with_event::<Watched>()
        .catchup_from_aggregate(stream.clone())
        .spawn(&system)
        .await;
    append(&system.event_store(), &stream, 1, Watched::EVENT_TYPE).await;
    first
        .counted(1, "the running projector must project the appended event")
        .await;
    let (system_ref, scope) = (&system, &OffsetScope::from(&id));
    until(
        "the running projector settling the appended event",
        move || async move { recorded(system_ref, scope).await == Some(1) },
    )
    .await;

    // Then
    assert!(
        second_handle.is_active().await,
        "the second spawn's handle must name the projector already running"
    );
    assert_eq!(
        (first.activations(), second.activations()),
        (1, 0),
        "a second spawn of a running projection must join it rather than start a projector \
         of its own — (first producer's activations, second producer's activations)"
    );
    assert_eq!(
        (first.applied(), second.applied()),
        (1, 0),
        "one projector, so the event is projected once — (first, second)"
    );
}

/// The registry keeps the declaration it was first given: later activations
/// of the projection are built from it, not from the one a joining spawn
/// carried.
#[tokio::test(start_paused = true)]
async fn a_second_spawn_leaves_the_first_declaration_to_later_activations() {
    // Given: a projector that passivates, and a second spawn of its id
    let system = system_on(without_default_idle().await, Watcher::Disabled);
    let id = ProjectionId::new("projection-activator-join-declaration");
    let stream = AggregateId::new("projection-activator-join-declaration");
    let first = Projection::default();
    let first_handle = ProjectorProps::new(id.clone(), first.producer())
        .with_event::<Watched>()
        .catchup_from_aggregate(stream.clone())
        .with_idle_timeout(IdleTimeout::After(IDLE))
        .spawn(&system)
        .await;
    let second = Projection::default();
    let _second_handle = ProjectorProps::new(id.clone(), second.producer())
        .with_event::<Watched>()
        .catchup_from_aggregate(stream.clone())
        .with_idle_timeout(IdleTimeout::After(IDLE))
        .spawn(&system)
        .await;
    tokio::time::sleep(QUIET).await;
    let handle_ref = &first_handle;
    until(
        "the projector stopping on its idle timeout",
        move || async move { !handle_ref.is_active().await },
    )
    .await;

    // When: an event it follows wakes it
    append(&system.event_store(), &stream, 1, Watched::EVENT_TYPE).await;
    first
        .counted(
            1,
            "the woken projector must be built from the first declaration",
        )
        .await;

    // Then
    assert_eq!(
        (first.activations(), second.activations()),
        (2, 0),
        "the activation that woke must be built from the declaration the projection was \
         first registered with — (first producer's activations, second producer's activations)"
    );
}

// Scenario: stopping through the handle takes the projector out

/// The watcher runs, so both paths that could revive a registered projector
/// see the later event; it is waited on until it has read past it.
#[tokio::test(start_paused = true)]
async fn a_projector_stopped_through_its_handle_is_not_activated_by_later_appends() {
    // Given: a running projector that has projected one event
    let system = system_on(
        without_default_idle().await,
        Watcher::Every(WATCHER_INTERVAL),
    );
    let journal = system.event_store();
    let id = ProjectionId::new("projection-activator-stop");
    let stream = AggregateId::new("projection-activator-stop");
    let projection = Projection::default();
    let handle = ProjectorProps::new(id.clone(), projection.producer())
        .with_event::<Watched>()
        .catchup_from_aggregate(stream.clone())
        .spawn(&system)
        .await;
    append(&journal, &stream, 1, Watched::EVENT_TYPE).await;
    projection
        .counted(1, "the projector must project the first event")
        .await;

    // When
    tokio::time::timeout(PROBE_TIMEOUT, handle.stop())
        .await
        .expect("stopping through the handle must finish once the projector has stopped");

    // Then
    assert!(
        !handle.is_active().await,
        "a projector stopped through its handle must be out of the registry"
    );

    // When: an event it followed is appended, and both paths have had it
    let head = append(&journal, &stream, 2, Watched::EVENT_TYPE).await;
    watcher_has_had_a_read_past(&system, head).await;
    tokio::time::sleep(QUIET).await;

    // Then
    assert_eq!(
        (projection.activations(), projection.applied()),
        (1, 1),
        "a projector stopped through its handle must not be activated again by what is \
         appended afterwards — (activations, applied)"
    );
    assert_eq!(
        recorded(&system, &OffsetScope::from(&id)).await,
        Some(1),
        "nothing may have carried the stopped projector's offset past the first event"
    );
}

// Scenario: the watcher's interval is configurable and it can be disabled

#[tokio::test(start_paused = true)]
async fn a_system_without_the_watcher_leaves_an_unannounced_write_unread() {
    // Given: a passivated projector on a system with the watcher disabled
    let system = system_on(without_default_idle().await, Watcher::Disabled);
    let stream = AggregateId::new("projection-activator-watcher-disabled");
    let projection = Projection::default();
    let handle = ProjectorProps::new(
        ProjectionId::new("projection-activator-watcher-disabled"),
        projection.producer(),
    )
    .with_event::<Watched>()
    .catchup_from_aggregate(stream.clone())
    .with_idle_timeout(IdleTimeout::After(IDLE))
    .spawn(&system)
    .await;
    append(&system.event_store(), &stream, 1, Watched::EVENT_TYPE).await;
    projection
        .counted(1, "the projector must catch up with the first event")
        .await;
    tokio::time::sleep(QUIET).await;
    let handle_ref = &handle;
    until(
        "the projector stopping on its idle timeout",
        move || async move { !handle_ref.is_active().await },
    )
    .await;

    // When: a write no notification announces, and far longer than any
    // watcher interval
    let unannounced = append(
        &system.persistence().event_store(),
        &stream,
        2,
        Watched::EVENT_TYPE,
    )
    .await;
    tokio::time::sleep(WATCHER_HORIZON).await;

    // Then
    assert_eq!(
        (projection.activations(), projection.applied()),
        (1, 1),
        "with the watcher disabled, nothing may pick up a write no notification announced \
         — (activations, applied)"
    );
    assert_eq!(
        system
            .persistence()
            .event_store()
            .head()
            .await
            .expect("reading the head of the in-memory journal must succeed"),
        Some(unannounced),
        "the unannounced write must be in the journal, left unread rather than lost"
    );
}

/// The default interval is measured from the moment the activator starts — the
/// first spawn — since that is the earliest any watcher could read.
#[tokio::test(start_paused = true)]
async fn the_default_watcher_waits_an_order_of_magnitude_longer_than_a_projector_poll() {
    // Given: a passivated projector on a system left at the default watcher
    let system = system_on(without_default_idle().await, Watcher::Default);
    let stream = AggregateId::new("projection-activator-watcher-default");
    let projection = Projection::default();
    let started_at = Instant::now();
    let handle = ProjectorProps::new(
        ProjectionId::new("projection-activator-watcher-default"),
        projection.producer(),
    )
    .with_event::<Watched>()
    .catchup_from_aggregate(stream.clone())
    .with_idle_timeout(IdleTimeout::After(IDLE))
    .spawn(&system)
    .await;
    append(&system.event_store(), &stream, 1, Watched::EVENT_TYPE).await;
    projection
        .counted(1, "the projector must catch up with the first event")
        .await;
    tokio::time::sleep(QUIET).await;
    let handle_ref = &handle;
    until(
        "the projector stopping on its idle timeout",
        move || async move { !handle_ref.is_active().await },
    )
    .await;

    // When: a write no notification announces
    append(
        &system.persistence().event_store(),
        &stream,
        2,
        Watched::EVENT_TYPE,
    )
    .await;
    let projection_ref = &projection;
    until_within(
        "the default watcher picking up the unannounced write",
        WATCHER_HORIZON,
        move || async move { projection_ref.applied() >= 2 },
    )
    .await;
    let picked_up_after = started_at.elapsed();

    // Then
    let order_of_magnitude = DEFAULT_POLL_INTERVAL * 10;
    assert!(
        picked_up_after >= order_of_magnitude,
        "the default watcher picked the write up {picked_up_after:?} after the activator \
         started; its interval must be at least {order_of_magnitude:?}, an order of \
         magnitude above a projector's {DEFAULT_POLL_INTERVAL:?} poll"
    );
}

// Scenario: history from before the activator started does not wake a
// projector whose position already covers it

/// The journal and the projector's recorded position are an earlier run's: its
/// two records, interleaved with another stream's, are covered by the offset,
/// so the first activation reads nothing and settles nothing. A watcher that
/// read from the journal's beginning would report both records again, and
/// nothing this run settled would weigh against them, so the projector would be
/// woken for them — once, or over and over each time it passivated again having
/// read nothing. The watcher reads from the journal's head as the activator
/// started; what lies before it is the first activation's catch-up to read.
///
/// The watcher is shown to be running all along by a write it alone can learn
/// of, appended after the activator started: that one wakes the projector.
#[tokio::test(start_paused = true)]
async fn history_from_before_the_activator_started_does_not_wake_a_projector_it_covers() {
    // Given: a journal and a recorded position from an earlier run
    let system = system_on(
        without_default_idle().await,
        Watcher::Every(WATCHER_INTERVAL),
    );
    let earlier_run = system.persistence().event_store();
    let id = ProjectionId::new("projection-activator-restart");
    let followed = AggregateId::new("projection-activator-restart");
    let other = AggregateId::new("projection-activator-restart-other");
    append(&earlier_run, &other, 1, Watched::EVENT_TYPE).await;
    append(&earlier_run, &followed, 1, Watched::EVENT_TYPE).await;
    append(&earlier_run, &other, 2, Watched::EVENT_TYPE).await;
    append(&earlier_run, &followed, 2, Watched::EVENT_TYPE).await;
    let head = append(&earlier_run, &other, 3, Watched::EVENT_TYPE).await;
    system
        .persistence()
        .projection_offsets()
        .save(&OffsetScope::from(&id), 2, None)
        .await
        .expect("recording the earlier run's position must succeed");

    // When: the projector is spawned — starting the activator and its watcher —
    // and the watcher has had reads since
    let projection = Projection::default();
    let handle = ProjectorProps::new(id.clone(), projection.producer())
        .with_event::<Watched>()
        .catchup_from_aggregate(followed.clone())
        .with_idle_timeout(IdleTimeout::After(IDLE))
        .spawn(&system)
        .await;
    watcher_has_had_a_read_past(&system, head).await;
    tokio::time::sleep(QUIET).await;

    // Then
    assert!(
        !handle.is_active().await,
        "a projector with nothing new to read must stay passivated"
    );
    assert_eq!(
        (projection.activations(), projection.applied()),
        (1, 0),
        "records from before the activator started, which the projector's recorded \
         position already covers, must not activate it again — (activations, applied)"
    );

    // When: a write only the watcher can learn of is appended to its stream
    append(&earlier_run, &followed, 3, Watched::EVENT_TYPE).await;

    // Then: the watcher, reading on from where it started, wakes the projector
    projection
        .counted(
            1,
            "the watcher must learn of a write appended after the activator started",
        )
        .await;
    assert_eq!(
        projection.activations(),
        2,
        "the write after the activator started must be projected by a second activation"
    );
}

// Scenario: an activation that fails is tried again without another append

/// The watcher is disabled and nothing more is appended after the one event, so
/// what activates the projector once the store answers again can only be the
/// activator's own retry: the notification of that event was spent on the
/// failed activation.
#[tokio::test(start_paused = true)]
async fn an_activation_that_fails_is_tried_again_without_another_append() {
    // Given: a passivated projector, and an offset store that refuses loads
    let offsets = FlakyOffsets::default();
    let system = flaky_system(without_default_idle().await, &offsets);
    let journal = system.event_store();
    let id = ProjectionId::new("projection-activator-retry");
    let stream = AggregateId::new("projection-activator-retry");
    let projection = Projection::default();
    let handle = ProjectorProps::new(id.clone(), projection.producer())
        .with_event::<Watched>()
        .catchup_from_global()
        .with_idle_timeout(IdleTimeout::After(IDLE))
        .spawn(&system)
        .await;
    let first = append(&journal, &stream, 1, Watched::EVENT_TYPE).await;
    projection
        .counted(1, "the projector must catch up with the first event")
        .await;
    tokio::time::sleep(QUIET).await;
    let handle_ref = &handle;
    until(
        "the projector stopping on its idle timeout",
        move || async move { !handle_ref.is_active().await },
    )
    .await;
    offsets.fail_loads(true);

    // When: an event it follows is appended, and its activation fails
    let second = append(&journal, &stream, 2, Watched::EVENT_TYPE).await;
    let offsets_ref = &offsets;
    until(
        "the activation being tried, and tried again, against the failing store",
        move || async move { offsets_ref.refused() >= 2 },
    )
    .await;
    assert_eq!(
        (projection.activations(), projection.applied()),
        (1, 1),
        "no projector can start while the offset store refuses its load — \
         (activations, applied)"
    );

    // When: the store recovers, with nothing further appended
    offsets.fail_loads(false);

    // Then: the projector is activated again and projects the event
    projection
        .counted(
            2,
            "the activator must try the failed activation again by itself",
        )
        .await;
    let (system_ref, scope) = (&system, &OffsetScope::from(&id));
    until(
        "the projector settling the second event",
        move || async move { recorded(system_ref, scope).await == Some(second) },
    )
    .await;
    assert_eq!(
        recorded(&system, scope).await.filter(|at| *at > first),
        Some(second),
        "the offset must have moved on from the first event to the second"
    );
}

// Scenario: an event a first read could not settle is not left behind

/// The event was journaled before any projector was spawned, so no
/// notification announced it and the watcher is disabled: the activator knows
/// of nothing owed. The first read hands it to a projector that refuses it, and
/// the poller waits out its interval — longer than the idle timeout — before
/// reading again. The activation has then neither settled a batch nor read to
/// the end of the journal when the idle timeout stops it, so the activator
/// cannot take the stop for a passivation and activates it again once the
/// poll interval has passed.
#[tokio::test(start_paused = true)]
async fn an_event_the_first_read_could_not_settle_is_read_again_after_an_idle_stop() {
    // Given: an event in the journal, and a projector that refuses it once
    let system = system_on(without_default_idle().await, Watcher::Disabled);
    let journal = system.event_store();
    let id = ProjectionId::new("projection-activator-first-read");
    let stream = AggregateId::new("projection-activator-first-read");
    let before_spawn = append(&journal, &stream, 1, Watched::EVENT_TYPE).await;
    let projection = Projection::default();
    projection.refuse_next(1);

    // When: the projector is spawned, and its idle timeout is shorter than the
    // poll interval it waits out before reading the event again
    assert!(IDLE < DEFAULT_POLL_INTERVAL);
    let _handle = ProjectorProps::new(id.clone(), projection.producer())
        .with_event::<Watched>()
        .catchup_from_global()
        .with_idle_timeout(IdleTimeout::After(IDLE))
        .spawn(&system)
        .await;

    // Then: the event is projected, by a later activation
    projection
        .counted(
            1,
            "the event the first read could not settle must be projected",
        )
        .await;
    assert!(
        projection.activations() >= 2,
        "the event can only have been projected by an activation after the one that \
         refused it, got {} activation(s)",
        projection.activations()
    );
    let (system_ref, scope) = (&system, &OffsetScope::from(&id));
    until("the projector settling the event", move || async move {
        recorded(system_ref, scope).await == Some(before_spawn)
    })
    .await;
}

// Scenario: a first read that fails is read again after an idle stop

/// The event was journaled before any projector was spawned, so no notification
/// announced it, and the watcher is disabled. The first reads fail, so no batch
/// ever reaches the projector and no read reaches the end of the journal; the
/// poller waits out an interval longer than the idle timeout before reading
/// again. The activation has reported neither a settled batch nor a caught-up
/// read when the idle timeout stops it, so the activator activates it again
/// once the poll interval has passed.
#[tokio::test(start_paused = true)]
async fn a_first_read_that_fails_is_read_again_after_an_idle_stop() {
    // Given: an event in the journal, and a journal that refuses reads
    let journal_store = FlakyJournal::default();
    let system = flaky_journal_system(without_default_idle().await, &journal_store);
    let journal = system.event_store();
    let id = ProjectionId::new("projection-activator-failed-first-read");
    let stream = AggregateId::new("projection-activator-failed-first-read");
    let before_spawn = append(&journal, &stream, 1, Watched::EVENT_TYPE).await;
    journal_store.fail_loads(true);
    let projection = Projection::default();

    // When: the projector is spawned, and its idle timeout is shorter than the
    // poll interval it waits out before reading again
    assert!(IDLE < DEFAULT_POLL_INTERVAL);
    let handle = ProjectorProps::new(id.clone(), projection.producer())
        .with_event::<Watched>()
        .catchup_from_global()
        .with_idle_timeout(IdleTimeout::After(IDLE))
        .spawn(&system)
        .await;
    let handle_ref = &handle;
    until(
        "the projector stopping on its idle timeout",
        move || async move { !handle_ref.is_active().await },
    )
    .await;
    assert_eq!(
        projection.applied(),
        0,
        "no record can be projected while every read fails"
    );

    // And: the journal recovers, with nothing further appended
    journal_store.fail_loads(false);

    // Then: the projector is activated again and projects the event
    projection
        .counted(
            1,
            "the activator must read again what the failed reads left unread",
        )
        .await;
    assert!(
        projection.activations() >= 2,
        "the event can only have been projected by an activation after the one whose \
         reads failed, got {} activation(s)",
        projection.activations()
    );
    let (system_ref, scope) = (&system, &OffsetScope::from(&id));
    until("the projector settling the event", move || async move {
        recorded(system_ref, scope).await == Some(before_spawn)
    })
    .await;
    assert!(
        journal_store.refused() >= 1,
        "the journal must have refused the first read"
    );
}

// Scenario: an at-most-once offset save that failed does not repeat an event

/// Under at-most-once the offset is saved before a handler runs, so a batch
/// whose save failed has had no handler run and is redelivered. The projector
/// stops idle while the save keeps failing, and an activation restores its
/// position from the saved offset alone, so a handler that had run for the
/// unsaved batch would run for it a second time.
#[tokio::test(start_paused = true)]
async fn an_at_most_once_offset_save_that_failed_does_not_repeat_an_event() {
    // Given: an at-most-once projector with an idle timeout, over an offset
    // store whose saves fail
    let offsets = FlakyOffsets::default();
    offsets.fail_saves(true);
    let system = flaky_system(without_default_idle().await, &offsets);
    let journal = system.event_store();
    let id = ProjectionId::new("projection-activator-at-most-once-save");
    let stream = AggregateId::new("projection-activator-at-most-once-save");
    let projection = Projection::default();
    let _handle = ProjectorProps::new(id.clone(), projection.producer())
        .with_event::<Watched>()
        .catchup_from_global()
        .delivery_mode(DeliveryMode::AtMostOnce)
        .with_idle_timeout(IdleTimeout::After(IDLE))
        .spawn(&system)
        .await;

    // When: an event it follows is appended, its save fails, and the projector
    // stops idle
    let first = append(&journal, &stream, 1, Watched::EVENT_TYPE).await;
    let offsets_ref = &offsets;
    until(
        "the projector's offset save being refused",
        move || async move { offsets_ref.refused_saves() >= 1 },
    )
    .await;
    let projection_ref = &projection;
    until(
        "the projector stopping on its idle timeout and being activated again",
        move || async move { projection_ref.activations() >= 2 },
    )
    .await;
    assert_eq!(
        projection.applied(),
        0,
        "no handler may run for a batch whose offset save failed"
    );

    // And: the store recovers, and a second event is appended
    offsets.fail_saves(false);
    let second = append(&journal, &stream, 2, Watched::EVENT_TYPE).await;

    // Then: each event has been handled exactly once
    let (system_ref, scope) = (&system, &OffsetScope::from(&id));
    until("the projector settling both events", move || async move {
        recorded(system_ref, scope).await == Some(second)
    })
    .await;
    tokio::time::sleep(QUIET).await;
    assert_eq!(
        projection.applied(),
        2,
        "the first event ({first}) must reach the handler once, not once per \
         activation, and the second once"
    );
}

// Scenario: a poller restarted by its supervision starts no second projector

/// The poller's first read panics, which its supervision answers by restarting
/// it; the restarted poller reads again a poll interval later. A restart is
/// finished inside the supervised process and ends nothing, so the projector
/// that owns the poller runs on throughout: nothing may take the restart for
/// the projector having stopped and start a second one beside it. The
/// projector has no idle timeout, so no idle stop can be what activates it
/// again either.
#[tokio::test(start_paused = true)]
async fn a_poller_restarted_by_its_supervision_starts_no_second_projector() {
    // Given: an event in the journal, and a journal whose next read panics
    let journal_store = PanickyJournal::default();
    let system = panicky_journal_system(without_default_idle().await, &journal_store);
    let journal = system.event_store();
    let id = ProjectionId::new("projection-activator-poller-restart");
    let stream = AggregateId::new("projection-activator-poller-restart");
    let event = append(&journal, &stream, 1, Watched::EVENT_TYPE).await;
    journal_store.panic_on_next_load();

    // When: the projector is spawned, its poller's first read panics, and the
    // restarted poller reads again
    let projection = Projection::default();
    let handle = ProjectorProps::new(id.clone(), projection.producer())
        .with_event::<Watched>()
        .catchup_from_global()
        .spawn(&system)
        .await;
    projection
        .counted(1, "the restarted poller must hand the event over")
        .await;
    assert_eq!(
        journal_store.panicked(),
        1,
        "the poller's first read must have panicked, for its supervision to restart it"
    );
    let (system_ref, scope) = (&system, &OffsetScope::from(&id));
    until("the projector settling the event", move || async move {
        recorded(system_ref, scope).await == Some(event)
    })
    .await;
    tokio::time::sleep(QUIET).await;

    // Then
    assert_eq!(
        (projection.activations(), projection.applied()),
        (1, 1),
        "a poller restarted by its supervision must leave the one projector running, so \
         the event is projected once and no second activation starts — \
         (activations, applied)"
    );
    assert!(
        handle.is_active().await,
        "the projector must still be running: its poller's restart is not its stop"
    );
}

// Scenario: a stream projector weighs what it is owed in its stream's units

/// The followed stream is written interleaved with another, so its records'
/// global positions run ahead of their stream sequences: its first record is
/// the journal's third. A projector following one stream keeps its offset in
/// that stream's sequence, so what the activator learns of an append to it is
/// weighed in the same unit. Weighed in global positions against a stream
/// offset, the settled first record would stay owed (3 past 1), and every idle
/// stop would activate the projector again to read nothing; weighed the other
/// way round, the second record (sequence 2) would look settled behind the
/// first's global position and wake nothing. The watcher is disabled, so every
/// activation is the notification's doing.
#[tokio::test(start_paused = true)]
async fn a_stream_projector_weighs_what_it_is_owed_in_its_streams_units() {
    // Given: a projector following one stream, written interleaved with another
    let system = system_on(without_default_idle().await, Watcher::Disabled);
    let journal = system.event_store();
    let id = ProjectionId::new("projection-activator-stream-units");
    let followed = AggregateId::new("projection-activator-stream-units");
    let other = AggregateId::new("projection-activator-stream-units-other");
    let projection = Projection::default();
    let handle = ProjectorProps::new(id.clone(), projection.producer())
        .with_event::<Watched>()
        .catchup_from_aggregate(followed.clone())
        .with_idle_timeout(IdleTimeout::After(IDLE))
        .spawn(&system)
        .await;
    append(&journal, &other, 1, Watched::EVENT_TYPE).await;
    append(&journal, &other, 2, Watched::EVENT_TYPE).await;
    let first = append(&journal, &followed, 1, Watched::EVENT_TYPE).await;
    assert!(
        first > 1,
        "the followed stream's first record must lie past global position 1, so its \
         global position and stream sequence differ"
    );

    // When: it settles that stream's record and passivates
    projection
        .counted(1, "the projector must project its stream's first record")
        .await;
    let (system_ref, scope) = (&system, &OffsetScope::from(&id));
    until(
        "the projector settling its stream's first record",
        move || async move { recorded(system_ref, scope).await == Some(1) },
    )
    .await;
    tokio::time::sleep(QUIET).await;
    let handle_ref = &handle;
    until(
        "the projector stopping on its idle timeout",
        move || async move { !handle_ref.is_active().await },
    )
    .await;
    tokio::time::sleep(QUIET).await;

    // Then: it stays passivated
    assert_eq!(
        (projection.activations(), projection.applied()),
        (1, 1),
        "a stream projector that settled its stream's record must not be activated again \
         for it because the record's global position ({first}) is past its stream offset \
         (1) — (activations, applied)"
    );

    // When: its stream is appended to again, at sequence 2 — behind the first
    // record's global position
    append(&journal, &followed, 2, Watched::EVENT_TYPE).await;

    // Then
    projection
        .counted(
            2,
            "an append to the followed stream past its settled sequence must wake it",
        )
        .await;
    assert_eq!(
        projection.activations(),
        2,
        "the stream's second record must be projected by a second activation"
    );
    until(
        "the projector settling its stream's second record",
        move || async move { recorded(system_ref, scope).await == Some(2) },
    )
    .await;
}

// Scenario: a read that failed does not hold back the next append's wake

/// The projector polls every [`LONG_POLL`], so what reads an append inside
/// [`NUDGED_WITHIN`] is the activator's request to read at once. The first
/// request's read fails and reports nothing; the next append's request must
/// still be told.
#[tokio::test(start_paused = true)]
async fn a_read_that_failed_does_not_hold_back_the_next_appends_wake() {
    // Given: a running projector polling every 10 s that has caught up with an
    // empty journal
    let journal_store = FlakyJournal::default();
    let system = flaky_journal_system(without_default_idle().await, &journal_store);
    let journal = system.event_store();
    let stream = AggregateId::new("projection-activator-failed-wake");
    let projection = Projection::default();
    let _handle = ProjectorProps::new(
        ProjectionId::new("projection-activator-failed-wake"),
        projection.producer(),
    )
    .with_event::<Watched>()
    .catchup_from_aggregate(stream.clone())
    .with_poll_interval(LONG_POLL)
    .spawn(&system)
    .await;
    tokio::time::sleep(QUIET).await;
    journal_store.fail_loads(true);

    // When: an append is announced and the read it wakes fails
    append(&journal, &stream, 1, Watched::EVENT_TYPE).await;
    let journal_store_ref = &journal_store;
    until(
        "the read the append woke being refused by the journal",
        move || async move { journal_store_ref.refused() >= 1 },
    )
    .await;
    tokio::time::sleep(QUIET).await;

    // And: the journal recovers, and another event is appended
    journal_store.fail_loads(false);
    let appended_at = Instant::now();
    append(&journal, &stream, 2, Watched::EVENT_TYPE).await;

    // Then: both are projected without waiting out the poll interval
    projection
        .counted(2, "the second append must wake the running projector")
        .await;
    let projected_in = appended_at.elapsed();
    assert!(
        projected_in < NUDGED_WITHIN,
        "the events took {projected_in:?} to be projected by a projector polling every \
         {LONG_POLL:?}; a read that failed must not keep the next append from waking it"
    );
}

// Scenario: a re-registered projection's restored offset is not owed again

/// The projection is stopped and spawned again before the watcher's second
/// read, which is the first to see the event: the registry entry the second
/// spawn made knows nothing of what the first settled, so it is the offset the
/// projector restores that tells the activator the event is not owed.
#[tokio::test(start_paused = true)]
async fn a_re_registered_projections_restored_offset_is_not_owed_again() {
    // Given: a projector that projected one event and was stopped through its
    // handle before the watcher read it
    let system = system_on(
        without_default_idle().await,
        Watcher::Every(WATCHER_INTERVAL),
    );
    let journal = system.event_store();
    let id = ProjectionId::new("projection-activator-re-registered");
    let stream = AggregateId::new("projection-activator-re-registered");
    let projection = Projection::default();
    let handle = ProjectorProps::new(id.clone(), projection.producer())
        .with_event::<Watched>()
        .catchup_from_global()
        .with_idle_timeout(IdleTimeout::After(IDLE))
        .spawn(&system)
        .await;
    tokio::time::sleep(PROBE_STEP * 4).await;
    append(&journal, &stream, 1, Watched::EVENT_TYPE).await;
    projection
        .counted(1, "the projector must project the event")
        .await;
    let (system_ref, scope) = (&system, &OffsetScope::from(&id));
    until("the projector settling the event", move || async move {
        recorded(system_ref, scope).await == Some(1)
    })
    .await;
    tokio::time::timeout(PROBE_TIMEOUT, handle.stop())
        .await
        .expect("stopping through the handle must finish once the projector has stopped");

    // When: the projection is spawned again, and the watcher reads the event
    let handle = ProjectorProps::new(id.clone(), projection.producer())
        .with_event::<Watched>()
        .catchup_from_global()
        .with_idle_timeout(IdleTimeout::After(IDLE))
        .spawn(&system)
        .await;
    tokio::time::sleep(WATCHER_INTERVAL * 3).await;

    // Then: the event it restored its offset past does not activate it again
    let (activations, applied) = (projection.activations(), projection.applied());
    tokio::time::sleep(WATCHER_INTERVAL * 3).await;
    assert_eq!(
        (projection.activations(), projection.applied()),
        (activations, applied),
        "an event the restored offset covers must not have the projector activated again and \
         again — (activations, applied)"
    );
    assert_eq!(
        (activations, applied),
        (2, 1),
        "the projection must have been activated once for each spawn, and projected the \
         event once — (activations, applied)"
    );
    assert!(!handle.is_active().await);
}

// Scenario: an offset that could not be loaded is loaded again after an idle
// stop, though the journal is empty

/// With nothing in the journal the poller's read finds nothing, whether or not
/// the offset was loaded; that is not what the activator may take for a first
/// activation settled, or the projector would never be started again.
#[tokio::test(start_paused = true)]
async fn an_offset_that_could_not_be_loaded_is_loaded_again_though_the_journal_is_empty() {
    // Given: an empty journal, and an offset store that refuses loads
    let offsets = FlakyOffsets::default();
    let system = flaky_system(without_default_idle().await, &offsets);
    offsets.fail_loads(true);
    let projection = Projection::default();

    // When: a projector is spawned, and stops on its idle timeout
    assert!(IDLE < DEFAULT_POLL_INTERVAL);
    let handle = ProjectorProps::new(
        ProjectionId::new("projection-activator-empty-journal"),
        projection.producer(),
    )
    .with_event::<Watched>()
    .catchup_from_global()
    .with_idle_timeout(IdleTimeout::After(IDLE))
    .spawn(&system)
    .await;
    let handle_ref = &handle;
    until(
        "the projector stopping on its idle timeout",
        move || async move { !handle_ref.is_active().await },
    )
    .await;

    // And: the offset store recovers, with nothing appended
    let offsets_ref = &offsets;
    until(
        "the activation being tried again against the failing store",
        move || async move { offsets_ref.refused() >= 2 },
    )
    .await;
    offsets.fail_loads(false);

    // Then: the activator starts the projector again, which builds its projector
    let projection_ref = &projection;
    until(
        "the projector being started with the recovered offset",
        move || async move { projection_ref.activations() >= 1 },
    )
    .await;
}

// Scenario: a watcher whose head could not be read passes over no write

/// The head is refused as the activator starts, and the projector passivates
/// after its first catch-up. A write the system's journal is not told of is
/// made before the watcher's interval has passed; a watcher that started at the
/// head it read an interval later would start past it.
#[tokio::test(start_paused = true)]
async fn a_watcher_whose_head_could_not_be_read_passes_over_no_write() {
    // Given: a journal whose head is refused as the activator starts
    let journal_store = HeadlessAtFirst::default();
    let system = EventSourceSystem::builder(without_default_idle().await)
        .with_persistence(PersistenceProvider::<HeadlessAtFirstStore>::new(
            CustomStore::builder()
                .eventstore(journal_store.clone())
                .projection_offsets(InMemoryOffsetStore::default())
                .build(),
        ))
        .with_projection_watcher_interval(WATCHER_INTERVAL)
        .build();
    let stream = AggregateId::new("projection-activator-headless");
    let projection = Projection::default();
    let handle = ProjectorProps::new(
        ProjectionId::new("projection-activator-headless"),
        projection.producer(),
    )
    .with_event::<Watched>()
    .catchup_from_global()
    .with_idle_timeout(IdleTimeout::After(IDLE))
    .spawn(&system)
    .await;
    let handle_ref = &handle;
    until(
        "the projector stopping on its idle timeout",
        move || async move { !handle_ref.is_active().await },
    )
    .await;

    // When: a write nothing announced is made, within the watcher's interval
    assert!(QUIET < WATCHER_INTERVAL);
    tokio::time::sleep(QUIET).await;
    journal_store
        .append(
            stream.as_str(),
            vec![AppendingEvent {
                sequence: 1,
                event_type: Watched::EVENT_TYPE,
                payload: Bytes::new(),
                occurred_at: jiff::Timestamp::now(),
            }],
        )
        .await
        .expect("appending to the in-memory journal must succeed");

    // Then: the watcher finds it and the projector projects it
    projection
        .counted(
            1,
            "the watcher must pick up the write made after the head was refused",
        )
        .await;
}
