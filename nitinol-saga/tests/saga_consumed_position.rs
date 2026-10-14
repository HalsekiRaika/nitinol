//! The upstream position a saga consumed is a fact on the saga's own stream.
//!
//! ```gherkin
//!   Scenario: a redelivered consumed input never reaches handle
//!     Given a saga that processed upstream seq N and persisted its decision
//!     When the saga restarts and upstream redelivers seq N
//!     Then Saga::handle is not called
//!     And the decision event is not duplicated
//!
//!   Scenario: decision and position are written by one append
//!     Given handle returned a decision for upstream seq N
//!     And the whole effect reaches the store as one atomic batch
//!     When the framework persists the result
//!     Then the decision and the seq N position are in that one append batch
//!     And neither is on the stream unless that batch landed
//!
//!   Scenario: a lost append among several withholds the position
//!     Given one handle call whose effect reaches the store as two appends
//!     And the opening one exhausted its retries
//!     When the closing append lands anyway
//!     Then no position is on the stream
//!     And handle runs again once the saga restarts
//!
//!   Scenario: a batch that never landed brings its input back
//!     Given the batch for upstream seq N exhausted its retries
//!     When the saga restarts and upstream redelivers seq N
//!     Then handle runs again (at-least-once; no position was written)
//! ```
//!
//! What changes with this file is the *means*, not the outcome: a redelivered
//! upstream record produced no second decision before either, but only because
//! the user's `handle` was written to absorb it.  Here the framework refuses to
//! call `handle` at all, so a saga whose `handle` is not idempotent is covered
//! too.
//!
//! `ConsumedPositionEvent` is `pub(crate)`, so every assertion below reads the
//! record the way any other consumer of the column does — by the canonical path
//! it is stored under (`nitinol.saga.consumed`), and the `oneof` its payload
//! carries: the upstream stream key, the sequence, and the canonical path of
//! the declared source the position settled.
//!
//! # How a failed append is driven, and when it is over
//!
//! An append a `handle` decision required that never landed leaves the input
//! unconsumed, and that is the whole of what it leaves: the failure is reported
//! and the delivery answered, so the incarnation stays alive — staying alive is
//! what lets the *next* input retry the decision, including an `End` whose
//! `Ended` fact did not land.  Nothing durable claims the record either, so
//! what re-opens it is a fresh host over the same journal: its shared cursor
//! sweeps that source from the position it was configured with, and the
//! instance it spawns drops only the records the journal names as consumed —
//! which such a failure left exactly where they were.  Every failure test below
//! spawns that successor by hand.
//!
//! Two things make those tests settled rather than sampled.  The store
//! decorator refuses exactly as many appends as the one under test attempts —
//! a `Persist` batch's staged retries, or the single attempt a schedule marker
//! gets — so the first incarnation exhausts the whole budget and the successor
//! meets a store that works.  And the first incarnation is stopped, and read
//! only once it has left the runtime's registry, so no retry of its can still
//! be in flight while the stream is being judged or while the successor is
//! writing.
//!
//! Negative claims ("this never reached handle") are never settled by a sleep:
//! each uses a later upstream record, the position of the input under test, or
//! the stop of the incarnation that could not write one, as a barrier — a stop
//! is answered on the saga's own mailbox, so it is as much a fact about what
//! has already finished as a row on the stream is.  The
//! poller delivers a stream in ascending order and `ask`s one record at a time,
//! so once the barrier has been handled the earlier record's fate is already
//! decided.
//!
//! Run with:
//! `cargo test -p nitinol-saga --test saga_consumed_position`.

#[path = "common/helpers.rs"]
mod common;
use common::{
    decode_consumed_fact, encode_consumed_fact, lifecycle_kind_of, readable_consumed_fact_of,
    ConsumedFactPayload, JsonCodec, LifecycleKind, CONSUMED_MARKER, SCHEDULE_MARKER,
};

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use futures_util::TryStreamExt;
use serde::{Deserialize, Serialize};

use nitinol_eventsource::codec::Codec;
use nitinol_eventsource::Event;
use nitinol_persistence::error::{AppendError, LoadError};
use nitinol_persistence::store::{EventStore, EventStream, InMemoryEventStore, Journal};
use nitinol_persistence::{
    AggregateId, AppendOutcome, AppendingEvent, EventType, Family, LoadQuery, LoadedEvent, TypeName,
};
use nitinol_runtime::ident::Pid;
use nitinol_runtime::ProcessSystem;
use nitinol_saga::{
    Deadline, DeadlineMessage, Saga, SagaContext, SagaEffect, SagaId, SagaManagerProps,
    SagaSubscribe, Subscription, TimerName,
};

// Domain

#[derive(Clone, Debug, Serialize, Deserialize)]
struct OrderPlaced {
    sku: String,
}

impl Event for OrderPlaced {
    const EVENT_TYPE: EventType = EventType::new(
        Family::new("consumed_position"),
        TypeName::new("OrderPlaced"),
    );
    type Codec = JsonCodec;
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ReservationRequested {
    sku: String,
}

impl Event for ReservationRequested {
    const EVENT_TYPE: EventType = EventType::new(
        Family::new("consumed_position"),
        TypeName::new("ReservationRequested"),
    );
    type Codec = JsonCodec;
}

/// Every `OrderPlaced` in this file belongs to the one saga each test spawns.
const SAGA_ID: &str = "consumed-position-saga";

/// The whole alphabet every saga in this file reacts to, bar the scheduling one:
/// a single observed source, no deadline.
#[derive(SagaSubscribe)]
enum Observed {
    Order(OrderPlaced),
}

/// The scheduling saga's promise to its future self.  It carries nothing — the
/// test only asks whether the firing happened at all — but it still names the
/// format it is written into the schedule fact as, because that is what a
/// persisted value owes.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Reminder;

struct ReminderCodec;

impl Codec<Reminder> for ReminderCodec {
    type Error = std::convert::Infallible;

    fn encode(_value: &Reminder) -> Result<Bytes, Self::Error> {
        Ok(Bytes::new())
    }

    fn decode(_payload: &[u8]) -> Result<Reminder, Self::Error> {
        Ok(Reminder)
    }
}

impl DeadlineMessage for Reminder {
    const TYPE_NAME: &'static str = "reminder";
    const CODEC_NAME: &'static str = "reminder-codec";
    type Codec = ReminderCodec;
}

/// The alphabet of [`SchedulingSaga`]: one observed source, plus the saga's own
/// time — the entry point with no upstream record behind it.
// The observed payload is decoded and delivered as the variant it belongs to;
// what this saga decides does not read it, which `dead_code` cannot tell apart
// from a field nothing fills.
#[allow(dead_code)]
#[derive(SagaSubscribe)]
enum TimerObserved {
    Order(OrderPlaced),
    #[subscribe(deadline)]
    Reminder(Reminder),
}

/// What one `Saga::handle` invocation saw.  The upstream coordinates identify
/// *which* record reached it, which is the whole question these tests ask.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Seen {
    source: String,
    sequence: u64,
}

type Handled = Arc<Mutex<Vec<Seen>>>;

fn record(handled: &Handled, ctx: &SagaContext) {
    handled
        .lock()
        .expect("handled buffer must not be poisoned")
        .push(Seen {
            source: ctx.upstream_aggregate_id().as_str().to_owned(),
            sequence: ctx.upstream_sequence(),
        });
}

fn seen(handled: &Handled) -> Vec<Seen> {
    handled
        .lock()
        .expect("handled buffer must not be poisoned")
        .clone()
}

/// How often one upstream record reached `handle`.
///
/// A failed append hands the record back to the poller, so "how many times"
/// is the observable that separates "the position was withheld" from "the
/// position was written anyway".
fn times_handled(handled: &Handled, source: &AggregateId, sequence: u64) -> usize {
    seen(handled)
        .iter()
        .filter(|s| s.source == source.as_str() && s.sequence == sequence)
        .count()
}

/// Persists one decision per upstream record it is handed.
struct DecidingSaga {
    handled: Handled,
}

#[async_trait]
impl Saga for DecidingSaga {
    type Subscribe = Observed;
    type Event = ReservationRequested;
    type Error = std::convert::Infallible;

    fn correlate(_input: &Self::Subscribe) -> Option<SagaId> {
        Some(SagaId::new(SAGA_ID))
    }

    async fn handle(
        &mut self,
        input: Self::Subscribe,
        ctx: &mut SagaContext,
    ) -> Result<SagaEffect<Self::Event>, Self::Error> {
        let Observed::Order(event) = input;
        record(&self.handled, ctx);
        Ok(SagaEffect::persist(ReservationRequested { sku: event.sku }))
    }
}

/// Returns an all-empty `Persist` — the batch itself carries nothing, but the
/// input that produced it still gets a consumed-position-only append.
struct EmptyPersistSaga {
    handled: Handled,
}

#[async_trait]
impl Saga for EmptyPersistSaga {
    type Subscribe = Observed;
    type Event = ReservationRequested;
    type Error = std::convert::Infallible;

    fn correlate(_input: &Self::Subscribe) -> Option<SagaId> {
        Some(SagaId::new(SAGA_ID))
    }

    async fn handle(
        &mut self,
        _input: Self::Subscribe,
        ctx: &mut SagaContext,
    ) -> Result<SagaEffect<Self::Event>, Self::Error> {
        record(&self.handled, ctx);
        Ok(SagaEffect::persist_all(vec![]))
    }
}

/// Returns an effect the interpreter cannot reach the store with in one
/// append: a `CancelSchedule` sits between two `Persist` branches, and it is a
/// hard boundary the `Persist × Persist` fold does not cross.
struct TwoBatchSaga {
    handled: Handled,
}

#[async_trait]
impl Saga for TwoBatchSaga {
    type Subscribe = Observed;
    type Event = ReservationRequested;
    type Error = std::convert::Infallible;

    fn correlate(_input: &Self::Subscribe) -> Option<SagaId> {
        Some(SagaId::new(SAGA_ID))
    }

    async fn handle(
        &mut self,
        input: Self::Subscribe,
        ctx: &mut SagaContext,
    ) -> Result<SagaEffect<Self::Event>, Self::Error> {
        let Observed::Order(event) = input;
        record(&self.handled, ctx);
        // This saga's alphabet declares no deadline, so it holds no
        // `Deadline<M>` to build the cancellation from — the public
        // `CancelSchedule` variant is the form a saga with no component has.
        Ok(SagaEffect::Sequence(vec![
            SagaEffect::persist(ReservationRequested {
                sku: event.sku.clone(),
            }),
            SagaEffect::CancelSchedule(TimerName::new("no-such-timer")),
            SagaEffect::persist(ReservationRequested { sku: event.sku }),
        ]))
    }
}

/// Returns `Persist.then_end().combine(Persist)`: `End` sits between two
/// `Persist` branches, so the physically last element of the `Sequence` is
/// never interpreted — `End`'s own `Ended` append is what actually closes the
/// input out.
struct PersistThenEndSaga {
    handled: Handled,
}

#[async_trait]
impl Saga for PersistThenEndSaga {
    type Subscribe = Observed;
    type Event = ReservationRequested;
    type Error = std::convert::Infallible;

    fn correlate(_input: &Self::Subscribe) -> Option<SagaId> {
        Some(SagaId::new(SAGA_ID))
    }

    async fn handle(
        &mut self,
        input: Self::Subscribe,
        ctx: &mut SagaContext,
    ) -> Result<SagaEffect<Self::Event>, Self::Error> {
        let Observed::Order(event) = input;
        record(&self.handled, ctx);
        Ok(SagaEffect::persist(ReservationRequested {
            sku: event.sku.clone(),
        })
        .then_end()
        .combine(SagaEffect::persist(ReservationRequested {
            sku: format!("{}-AFTER-END", event.sku),
        })))
    }
}

/// Sets a deadline from the upstream record it handles, then persists a
/// decision when that deadline fires.  The observed arm persists nothing, so
/// the one `ReservationRequested` on the stream is the deadline's — the entry
/// point with no upstream record behind it, and so no position to commit.
struct SchedulingSaga {
    deadline: Deadline<Reminder>,
    fired: Arc<AtomicU64>,
}

#[async_trait]
impl Saga for SchedulingSaga {
    type Subscribe = TimerObserved;
    type Event = ReservationRequested;
    type Error = std::convert::Infallible;

    fn correlate(input: &Self::Subscribe) -> Option<SagaId> {
        match input {
            TimerObserved::Order(_) => Some(SagaId::new(SAGA_ID)),
            // A firing is addressed to the instance that set it and never
            // travels the subscription path, so this arm is unreachable.
            TimerObserved::Reminder(_) => None,
        }
    }

    async fn handle(
        &mut self,
        input: Self::Subscribe,
        _ctx: &mut SagaContext,
    ) -> Result<SagaEffect<Self::Event>, Self::Error> {
        match input {
            TimerObserved::Order(_) => Ok(self.deadline.set(
                TimerName::new("reminder"),
                Duration::from_millis(50),
                Reminder,
            )),
            TimerObserved::Reminder(_) => {
                self.fired.fetch_add(1, Ordering::SeqCst);
                Ok(SagaEffect::persist(ReservationRequested {
                    sku: "SKU-FROM-TIMER".to_owned(),
                }))
            }
        }
    }
}

// Store decorators

#[derive(Clone, Debug)]
struct RecordedEvent {
    sequence: u64,
    event_type: EventType,
    payload: Bytes,
    occurred_at: jiff::Timestamp,
}

type Batches = Arc<Mutex<Vec<Vec<RecordedEvent>>>>;

/// Records one entry per successful `append`, preserving the batch boundary —
/// the only place the atomicity of "decision + position" is observable.
#[derive(Clone)]
struct RecordingStore {
    inner: InMemoryEventStore,
    batches: Batches,
}

#[async_trait]
impl EventStore for RecordingStore {
    async fn append(
        &self,
        key: &str,
        events: Vec<AppendingEvent>,
    ) -> Result<AppendOutcome, AppendError> {
        let recorded: Vec<RecordedEvent> = events
            .iter()
            .map(|e| RecordedEvent {
                sequence: e.sequence,
                event_type: e.event_type,
                payload: e.payload.clone(),
                occurred_at: e.occurred_at,
            })
            .collect();
        // Only a committed append produced a batch boundary in the stream.
        let outcome = self.inner.append(key, events).await?;
        self.batches
            .lock()
            .expect("recorded batches must not be poisoned")
            .push(recorded);
        Ok(outcome)
    }

    async fn load(&self, query: LoadQuery) -> Result<EventStream<'_>, LoadError> {
        self.inner.load(query).await
    }
}

/// Rejects the first `budget` appends `reject` answers `true` for, and accepts
/// everything else — including every append of that same shape once the budget
/// is spent.
///
/// One decorator rather than one type per injected failure: every failure in
/// this file is "this particular append does not land, this many times", and
/// only *which* append and *how often* vary.  Spending a budget that matches
/// the staged retries of the append under test is what makes the recovery
/// deterministic — the attempt under test exhausts it, and the redelivery that
/// its failure causes meets a store that works, so each assertion below is
/// about a settled stream rather than a moment in a retry loop.
#[derive(Clone)]
struct FailFirstAppendsStore {
    inner: InMemoryEventStore,
    reject: fn(&[AppendingEvent]) -> bool,
    /// Shared, so a clone of this handle spends the same budget: a store that
    /// refused `budget` appends per handle would refuse more of them than the
    /// clause names.
    budget: Arc<AtomicU64>,
}

impl FailFirstAppendsStore {
    /// A saga store that refuses the first `budget` appends `reject` answers
    /// `true` for.  Handed back as the erased journal a saga is spawned with,
    /// because that is the only shape a test ever needs it in.
    fn refusing(reject: fn(&[AppendingEvent]) -> bool, budget: u64) -> Journal {
        Journal::from(Self {
            inner: InMemoryEventStore::default(),
            reject,
            budget: Arc::new(AtomicU64::new(budget)),
        })
    }

    /// Answers whether this append is refused, spending one unit of the budget
    /// when it is.
    fn refuses(&self, events: &[AppendingEvent]) -> bool {
        if !(self.reject)(events) {
            return false;
        }
        self.budget
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |left| {
                (left > 0).then(|| left - 1)
            })
            .is_ok()
    }
}

#[async_trait]
impl EventStore for FailFirstAppendsStore {
    async fn append(
        &self,
        key: &str,
        events: Vec<AppendingEvent>,
    ) -> Result<AppendOutcome, AppendError> {
        if self.refuses(&events) {
            return Err(AppendError::Backend(
                "injected: this append never landed".into(),
            ));
        }
        self.inner.append(key, events).await
    }

    async fn load(&self, query: LoadQuery) -> Result<EventStream<'_>, LoadError> {
        self.inner.load(query).await
    }
}

/// How many attempts one persist batch makes before giving its input back to
/// redelivery — the default `RetryPolicy`'s `max_attempts`.
///
/// A budget of exactly this many refusals exhausts one `Persist` batch and
/// nothing beyond it.
const STAGED_RETRIES: u64 = 3;

/// A budget no run of these tests can spend: the append under test never lands.
const NEVER: u64 = u64::MAX;

/// Carries one of the saga's own decisions.
fn carries_a_decision(events: &[AppendingEvent]) -> bool {
    events
        .iter()
        .any(|e| e.event_type == ReservationRequested::EVENT_TYPE)
}

/// Carries the consumed position of the input it closes out.
///
/// Within a `Sequence` only the append that lands last for the input carries
/// one, so this names the *closing* append and no other.
fn carries_a_position(events: &[AppendingEvent]) -> bool {
    events.iter().any(|e| e.event_type == CONSUMED_MARKER)
}

/// Carries a schedule marker — the `CancelSchedule` append, which unlike a
/// persist batch is attempted once and never retried.
fn carries_a_schedule_marker(events: &[AppendingEvent]) -> bool {
    events.iter().any(|e| e.event_type == SCHEDULE_MARKER)
}

/// Yields every stored record twice in one load.
///
/// At-least-once delivery lets the same upstream record reach a *running*
/// saga more than once; this reproduces that deterministically, without the
/// restart the other tests use, so the position the interpreter holds in
/// memory is what the second delivery is judged against.
#[derive(Clone)]
struct DuplicatingStore {
    inner: InMemoryEventStore,
}

#[async_trait]
impl EventStore for DuplicatingStore {
    async fn append(
        &self,
        key: &str,
        events: Vec<AppendingEvent>,
    ) -> Result<AppendOutcome, AppendError> {
        self.inner.append(key, events).await
    }

    async fn load(&self, query: LoadQuery) -> Result<EventStream<'_>, LoadError> {
        let loaded: Vec<LoadedEvent> = self.inner.load(query).await?.try_collect().await?;
        let doubled: Vec<Result<LoadedEvent, LoadError>> = loaded
            .into_iter()
            .flat_map(|event| [Ok(event.clone()), Ok(event)])
            .collect();
        Ok(Box::pin(futures_util::stream::iter(doubled)))
    }
}

type Queries = Arc<Mutex<Vec<LoadQuery>>>;

/// Records the [`LoadQuery`] of every poll, so the position a fresh
/// incarnation *asks upstream from* — not merely what it does with what
/// arrives — is observable.
#[derive(Clone)]
struct QueryRecordingStore {
    inner: InMemoryEventStore,
    queries: Queries,
}

#[async_trait]
impl EventStore for QueryRecordingStore {
    async fn append(
        &self,
        key: &str,
        events: Vec<AppendingEvent>,
    ) -> Result<AppendOutcome, AppendError> {
        self.inner.append(key, events).await
    }

    async fn load(&self, query: LoadQuery) -> Result<EventStream<'_>, LoadError> {
        self.queries
            .lock()
            .expect("recorded queries must not be poisoned")
            .push(query.clone());
        self.inner.load(query).await
    }
}

fn take_queries(queries: &Queries) -> Vec<LoadQuery> {
    std::mem::take(
        &mut *queries
            .lock()
            .expect("recorded queries must not be poisoned"),
    )
}

// Store helpers

async fn append_order_placed(store: &Journal, source: &AggregateId, sequence: u64, sku: &str) {
    let payload = serde_json::to_vec(&OrderPlaced {
        sku: sku.to_owned(),
    })
    .map(Bytes::from)
    .expect("encode OrderPlaced must succeed");
    store
        .append(
            source.as_str(),
            vec![AppendingEvent {
                sequence,
                event_type: OrderPlaced::EVENT_TYPE,
                payload,
                occurred_at: jiff::Timestamp::now(),
            }],
        )
        .await
        .expect("append OrderPlaced must succeed");
}

/// Write a raw row onto the saga's own stream, the way a previous incarnation
/// would have left it behind.
async fn seed_saga_row(
    store: &Journal,
    saga_id: &SagaId,
    sequence: u64,
    event_type: EventType,
    payload: Bytes,
) {
    store
        .append(
            saga_id.as_str(),
            vec![AppendingEvent {
                sequence,
                event_type,
                payload,
                occurred_at: jiff::Timestamp::now(),
            }],
        )
        .await
        .expect("seeding a saga row must succeed");
}

/// The position every saga in this file writes for `sequence` of `source`.
///
/// `OrderPlaced` is the one declared source any of them observes, so the
/// `source_event_type` field is the same throughout — but it is still named
/// rather than left out: a position that could not say which source it settled
/// would be a corrupted row, and one naming the wrong source would cover
/// records nobody consumed.
fn consumed(source: &AggregateId, sequence: u64) -> ConsumedFactPayload {
    ConsumedFactPayload {
        source: source.as_str().to_owned(),
        sequence,
        source_event_type: OrderPlaced::EVENT_TYPE.to_string(),
    }
}

async fn seed_consumed_position(
    store: &Journal,
    saga_id: &SagaId,
    sequence: u64,
    source: &AggregateId,
    upstream_sequence: u64,
) {
    seed_saga_row(
        store,
        saga_id,
        sequence,
        CONSUMED_MARKER,
        encode_consumed_fact(
            source.as_str(),
            upstream_sequence,
            &OrderPlaced::EVENT_TYPE.to_string(),
        ),
    )
    .await;
}

/// Append an event carrying `OrderPlaced::EVENT_TYPE` but an invalid JSON
/// payload, so the codec's `decode` call fails the same way a corrupted wire
/// record would — exercising the source transform's decode-failure path
/// instead of its decoded one.
async fn append_corrupt_order_placed(store: &Journal, source: &AggregateId, sequence: u64) {
    store
        .append(
            source.as_str(),
            vec![AppendingEvent {
                sequence,
                event_type: OrderPlaced::EVENT_TYPE,
                payload: Bytes::from_static(b"NOT-VALID-JSON"),
                occurred_at: jiff::Timestamp::now(),
            }],
        )
        .await
        .expect("append corrupt OrderPlaced must succeed");
}

async fn load_saga_events(store: &Journal, saga_id: &SagaId) -> Vec<LoadedEvent> {
    store
        .load(LoadQuery::by_stream(saga_id))
        .await
        .expect("load saga stream must succeed")
        .try_collect()
        .await
        .expect("collect saga events must succeed")
}

fn decisions(events: &[LoadedEvent]) -> Vec<&LoadedEvent> {
    events
        .iter()
        .filter(|e| e.event_type == ReservationRequested::EVENT_TYPE)
        .collect()
}

/// Every position the stream holds that can be read as one.
fn positions(events: &[LoadedEvent]) -> Vec<ConsumedFactPayload> {
    events
        .iter()
        .filter_map(readable_consumed_fact_of)
        .collect()
}

/// How many rows the stream carries under the consumed path, readable or not.
///
/// Counted apart from [`positions`] so an assertion can say "and nothing
/// unreadable was written here either" rather than only "every readable row is
/// the one expected".
fn position_rows(events: &[LoadedEvent]) -> usize {
    events
        .iter()
        .filter(|e| e.event_type == CONSUMED_MARKER)
        .count()
}

fn describe(events: &[LoadedEvent]) -> Vec<String> {
    events
        .iter()
        .map(|e| match readable_consumed_fact_of(e) {
            Some(p) => format!(
                "{}#{}: consumed({}, {}, {})",
                e.sequence, e.event_type, p.source, p.sequence, p.source_event_type
            ),
            None => format!("{}#{}", e.sequence, e.event_type),
        })
        .collect()
}

/// How long a positive barrier is given before the test calls it a failure.
const BARRIER_TIMEOUT: Duration = Duration::from_secs(10);
const POLL_INTERVAL: Duration = Duration::from_millis(25);

async fn wait_for_handled(handled: &Handled, expected: usize) -> Vec<Seen> {
    let deadline = std::time::Instant::now() + BARRIER_TIMEOUT;
    loop {
        let current = seen(handled);
        if current.len() >= expected {
            return current;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for {expected} handle invocations; saw {current:?}"
        );
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

/// Stop the host `proxy` addresses and wait until it has left the runtime's
/// registry.
///
/// An append a saga could not make does *not* take its instance down — staying
/// alive is what lets the next input retry the decision — so "the first
/// incarnation is finished with this record" has to be made true rather than
/// waited for.  A stop is delivered on the instance's own mailbox and the loop
/// is already inside the delivery, so the signal is not looked at until `recv`
/// returns: the host's pid leaving the registry therefore proves the whole
/// interpretation is over, every staged retry of the refused batch included —
/// a process is unregistered only after its children are, so the host is the
/// last of the fan-out to go.
///
/// Every failure test below goes through this before reading the stream or
/// spawning a successor.  Without it a retry still in flight could land a row
/// between the read and the assertion, or race the successor's own appends for
/// the same stream sequence.
async fn stop_and_wait<S: Saga>(ps: &ProcessSystem, proxy: &nitinol_saga::SagaManagerProxy<S>) {
    proxy.stop().await.expect("stop must succeed");
    wait_until_gone(ps, proxy.pid()).await;
}

/// The registry is what says a process is gone: it is unregistered as the last
/// step of its own termination, after the delivery it was in has returned and
/// its children — each source's poller, and the instances the host fronts —
/// have been stopped with it.
async fn wait_until_gone(ps: &ProcessSystem, pid: Pid) {
    let deadline = std::time::Instant::now() + BARRIER_TIMEOUT;
    loop {
        if ps.lookup(pid).await.is_none() {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for the stopped saga incarnation to leave the \
             registry; until it does, a retry of its could still land a row the \
             assertions below would read, or race the successor's appends"
        );
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

async fn wait_for_saga_rows(
    store: &Journal,
    saga_id: &SagaId,
    event_type: EventType,
    expected: usize,
) -> Vec<LoadedEvent> {
    let deadline = std::time::Instant::now() + BARRIER_TIMEOUT;
    loop {
        let events = load_saga_events(store, saga_id).await;
        if events.iter().filter(|e| e.event_type == event_type).count() >= expected {
            return events;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for {expected} `{event_type}` rows; stream held {:?}",
            describe(&events)
        );
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

async fn spawn_deciding(
    ps: &ProcessSystem,
    saga_store: &Journal,
    subscription: Subscription,
    handled: &Handled,
) -> nitinol_saga::SagaManagerProxy<DecidingSaga> {
    let handled = Arc::clone(handled);
    SagaManagerProps::<DecidingSaga>::new(saga_store.clone(), move |_components| DecidingSaga {
        handled: Arc::clone(&handled),
    })
    .observing::<OrderPlaced>(subscription)
    .spawn(ps)
    .await
}

async fn spawn_two_batch(
    ps: &ProcessSystem,
    saga_store: &Journal,
    subscription: Subscription,
    handled: &Handled,
) -> nitinol_saga::SagaManagerProxy<TwoBatchSaga> {
    let handled = Arc::clone(handled);
    SagaManagerProps::<TwoBatchSaga>::new(saga_store.clone(), move |_components| TwoBatchSaga {
        handled: Arc::clone(&handled),
    })
    .observing::<OrderPlaced>(subscription)
    .spawn(ps)
    .await
}

async fn spawn_persist_then_end(
    ps: &ProcessSystem,
    saga_store: &Journal,
    subscription: Subscription,
    handled: &Handled,
) -> nitinol_saga::SagaManagerProxy<PersistThenEndSaga> {
    let handled = Arc::clone(handled);
    SagaManagerProps::<PersistThenEndSaga>::new(saga_store.clone(), move |_components| {
        PersistThenEndSaga {
            handled: Arc::clone(&handled),
        }
    })
    .observing::<OrderPlaced>(subscription)
    .spawn(ps)
    .await
}

/// The one declared source of every saga in this file, read from the `source`
/// stream of `upstream_store` from the beginning.
fn from_source(upstream_store: &Journal, source: &AggregateId) -> Subscription {
    Subscription::stream(upstream_store, source)
}

/// The same source, read from every stream of `upstream_store` at once in the
/// store's own global order.
///
/// A store-wide cursor cannot be read as a per-stream position at all, so a
/// source bound this way cannot even in principle be nudged forward by what the
/// saga's own stream records — which makes it the sharpest shape for the tests
/// below, where what has to be observed is the framework's skip rather than a
/// cursor that never offered the record in the first place.
fn from_every_source(upstream_store: &Journal) -> Subscription {
    Subscription::global(upstream_store)
}

// CP-ATOMIC — the decision and the position reach the store as one append

/// Given a saga whose `handle` returns one decision for upstream seq 1,
/// When the framework persists the result,
/// Then a single append carries both the decision and the consumed position:
/// neither is ever in the store without the other, and the position closes the
/// batch.
#[tokio::test]
async fn the_decision_and_the_consumed_position_reach_the_store_in_one_append() {
    let ps = ProcessSystem::new().await;

    let upstream_store = Journal::from(InMemoryEventStore::default());
    let source = AggregateId::new("atomic-order");
    append_order_placed(&upstream_store, &source, 1, "SKU-1").await;

    let batches: Batches = Arc::new(Mutex::new(Vec::new()));
    let saga_store = Journal::from(RecordingStore {
        inner: InMemoryEventStore::default(),
        batches: Arc::clone(&batches),
    });
    let handled: Handled = Arc::new(Mutex::new(Vec::new()));

    let _proxy = spawn_deciding(
        &ps,
        &saga_store,
        from_source(&upstream_store, &source),
        &handled,
    )
    .await;

    let saga_id = SagaId::new(SAGA_ID);
    let events = wait_for_saga_rows(&saga_store, &saga_id, CONSUMED_MARKER, 1).await;

    let recorded = batches
        .lock()
        .expect("recorded batches must not be poisoned")
        .clone();
    assert_eq!(
        recorded.len(),
        1,
        "the position is part of the decision's batch, not a second write of its \
         own; stream held {:?}",
        describe(&events)
    );

    let batch = &recorded[0];
    let types: Vec<String> = batch.iter().map(|e| e.event_type.to_string()).collect();
    assert_eq!(
        types,
        vec![
            ReservationRequested::EVENT_TYPE.to_string(),
            CONSUMED_MARKER.to_string(),
        ],
        "the one append carries the decision and the position of the input it \
         was decided from, in that order: the position closes the batch, and \
         placed anywhere earlier it would shift every row after it; stream held \
         {:?}",
        describe(&events)
    );

    let position = batch.last().expect("the batch asserted above is not empty");
    assert_eq!(
        decode_consumed_fact(&position.payload),
        consumed(&source, 1),
        "the position names the upstream stream, the sequence handled and the \
         declared source it settled"
    );

    let stamp = batch[0].occurred_at;
    assert!(
        batch.iter().all(|e| e.occurred_at == stamp),
        "every row of one commit carries the commit's own timestamp; a position \
         stamped from a second clock reading would break the one signal a reader \
         has that the rows landed together"
    );
}

// CP-ATOMIC-FAIL / CP-ATLEASTONCE — a batch that never landed leaves no position

/// Given a store that rejects every append carrying the saga's decision,
/// When the saga handles the upstream record,
/// Then nothing at all is on the saga's stream — no decision, and above all no
/// consumed position: a position is only ever as durable as the decision it was
/// committed with.
#[tokio::test]
async fn a_batch_that_never_landed_writes_no_consumed_position() {
    let ps = ProcessSystem::new().await;

    let upstream_store = Journal::from(InMemoryEventStore::default());
    let source = AggregateId::new("failed-batch-order");
    append_order_placed(&upstream_store, &source, 1, "SKU-1").await;

    let saga_store = FailFirstAppendsStore::refusing(carries_a_decision, NEVER);
    let handled: Handled = Arc::new(Mutex::new(Vec::new()));

    let proxy = spawn_deciding(
        &ps,
        &saga_store,
        from_source(&upstream_store, &source),
        &handled,
    )
    .await;

    // Stopping the incarnation is the barrier: the signal waits behind the
    // delivery it is already inside, so once the pid is gone every staged
    // retry of the refused batch has been made and nothing else can land.
    let saga_id = SagaId::new(SAGA_ID);
    let barrier = wait_for_handled(&handled, 1).await;
    stop_and_wait(&ps, &proxy).await;
    let events = load_saga_events(&saga_store, &saga_id).await;

    assert_eq!(
        barrier,
        vec![Seen {
            source: source.as_str().to_owned(),
            sequence: 1,
        }],
        "the upstream record did reach handle — the store, not correlation, is \
         what stopped it; stream held {:?}",
        describe(&events)
    );
    assert!(
        decisions(&events).is_empty(),
        "the decision never landed; stream held {:?}",
        describe(&events)
    );
    assert!(
        positions(&events).is_empty(),
        "with no decision persisted the input is not consumed, so no position \
         may be on the stream; stream held {:?}",
        describe(&events)
    );
    assert!(
        events.is_empty(),
        "a batch that never landed leaves the saga's own stream exactly as it \
         was — the position is the row that would be tempting to write anyway, \
         and there is nothing else to write either; stream held {:?}",
        describe(&events)
    );
}

/// Given an input whose batch exhausted its retries before the saga stopped,
/// When a fresh incarnation is spawned and upstream redelivers it,
/// Then `handle` runs again — at-least-once, because no position was written —
/// and that attempt's batch carries the decision and the position together.
///
/// The store's budget is exactly one batch's staged retries, so the first
/// incarnation spends it and the successor meets a store that works.  Were the
/// first attempt credited with a position, `handle` would never see the record
/// again and the decision would be lost for good.
#[tokio::test]
async fn an_input_whose_batch_never_landed_is_handled_again_after_restart() {
    let ps = ProcessSystem::new().await;

    let upstream_store = Journal::from(InMemoryEventStore::default());
    let source = AggregateId::new("at-least-once-order");
    append_order_placed(&upstream_store, &source, 1, "SKU-1").await;

    let saga_store = FailFirstAppendsStore::refusing(carries_a_decision, STAGED_RETRIES);
    let handled: Handled = Arc::new(Mutex::new(Vec::new()));
    let saga_id = SagaId::new(SAGA_ID);

    let first = spawn_deciding(
        &ps,
        &saga_store,
        from_source(&upstream_store, &source),
        &handled,
    )
    .await;
    wait_for_handled(&handled, 1).await;
    stop_and_wait(&ps, &first).await;
    drop(first);

    let events = load_saga_events(&saga_store, &saga_id).await;
    assert!(
        events.is_empty(),
        "the first attempt's batch never landed, so it left neither a decision \
         nor a position behind; stream held {:?}",
        describe(&events)
    );

    let _second = spawn_deciding(
        &ps,
        &saga_store,
        from_source(&upstream_store, &source),
        &handled,
    )
    .await;

    let events = wait_for_saga_rows(&saga_store, &saga_id, CONSUMED_MARKER, 1).await;

    assert_eq!(
        times_handled(&handled, &source, 1),
        2,
        "the first attempt wrote no position, so redelivery must reach handle \
         again; invocations: {:?}",
        seen(&handled)
    );
    assert_eq!(
        decisions(&events).len(),
        1,
        "the re-run is what finally persists the decision; stream held {:?}",
        describe(&events)
    );
    assert_eq!(
        positions(&events),
        vec![consumed(&source, 1)],
        "and the position lands with it; stream held {:?}",
        describe(&events)
    );
}

// CP-SKIP — a consumed input never reaches handle again

/// Acceptance scenario 1.
///
/// Given a saga that handled upstream seq 1 and persisted its decision,
/// When it restarts and the subscription is configured from the beginning
/// again,
/// Then seq 1 never reaches `handle` and the decision is not duplicated.
///
/// Upstream seq 2 is the barrier: the poller delivers ascending, so once seq 2
/// has been handled a second delivery of seq 1 can no longer be pending.
#[tokio::test]
async fn a_consumed_input_never_reaches_handle_after_a_restart() {
    let ps = ProcessSystem::new().await;

    let upstream_store = Journal::from(InMemoryEventStore::default());
    let source = AggregateId::new("skip-order");
    append_order_placed(&upstream_store, &source, 1, "SKU-1").await;

    let saga_store = Journal::from(InMemoryEventStore::default());
    let handled: Handled = Arc::new(Mutex::new(Vec::new()));
    let saga_id = SagaId::new(SAGA_ID);

    let first = spawn_deciding(
        &ps,
        &saga_store,
        from_source(&upstream_store, &source),
        &handled,
    )
    .await;
    wait_for_saga_rows(&saga_store, &saga_id, CONSUMED_MARKER, 1).await;
    first.stop().await.expect("stop must succeed");
    drop(first);

    let _second = spawn_deciding(
        &ps,
        &saga_store,
        from_source(&upstream_store, &source),
        &handled,
    )
    .await;
    append_order_placed(&upstream_store, &source, 2, "SKU-2").await;

    let barrier = wait_for_handled(&handled, 2).await;
    let events = load_saga_events(&saga_store, &saga_id).await;

    assert_eq!(
        barrier.iter().map(|s| s.sequence).collect::<Vec<_>>(),
        vec![1, 2],
        "seq 1 belongs to the first incarnation alone; the second must see only \
         the input it has not consumed; stream held {:?}",
        describe(&events)
    );
    assert_eq!(
        decisions(&events).len(),
        2,
        "one decision per input handled — the redelivered seq 1 adds none; \
         stream held {:?}",
        describe(&events)
    );
}

/// The skip is the framework's, not the cursor's.
///
/// Given a `Global` subscription, whose resume position a per-source stream
/// sequence cannot be derived for, so redelivery of a consumed record really
/// does happen,
/// When the saga restarts,
/// Then the redelivered record is dropped before `handle`.
#[tokio::test]
async fn a_redelivered_consumed_input_is_dropped_even_when_the_cursor_replays_it() {
    let ps = ProcessSystem::new().await;

    let upstream_store = Journal::from(InMemoryEventStore::default());
    let source = AggregateId::new("global-skip-order");
    append_order_placed(&upstream_store, &source, 1, "SKU-1").await;

    let saga_store = Journal::from(InMemoryEventStore::default());
    let handled: Handled = Arc::new(Mutex::new(Vec::new()));
    let saga_id = SagaId::new(SAGA_ID);

    let first = spawn_deciding(
        &ps,
        &saga_store,
        from_every_source(&upstream_store),
        &handled,
    )
    .await;
    wait_for_saga_rows(&saga_store, &saga_id, CONSUMED_MARKER, 1).await;
    first.stop().await.expect("stop must succeed");
    drop(first);

    let _second = spawn_deciding(
        &ps,
        &saga_store,
        from_every_source(&upstream_store),
        &handled,
    )
    .await;
    append_order_placed(&upstream_store, &source, 2, "SKU-2").await;

    let barrier = wait_for_handled(&handled, 2).await;
    let events = load_saga_events(&saga_store, &saga_id).await;

    assert_eq!(
        barrier.iter().map(|s| s.sequence).collect::<Vec<_>>(),
        vec![1, 2],
        "a Global cursor starts over from its configured position, so seq 1 is \
         re-read from the store; what stops it is the position, not the cursor; \
         stream held {:?}",
        describe(&events)
    );
    assert_eq!(
        decisions(&events).len(),
        2,
        "the replayed record must add no second decision; stream held {:?}",
        describe(&events)
    );
}

/// A corrupt upstream record is skipped, and nothing durable is written for it
/// — not even a position row of its own where one already names it.
///
/// Given a saga stream already carrying a consumed position for the upstream
/// position the corrupt record sits at,
/// When a fresh incarnation subscribes with a `Global` cursor — which keeps its
/// configured position and so really does redeliver the corrupt record —
/// Then the record never reaches `handle`, and the position it sits at is still
/// named by exactly one row: an undecodable record is reported and skipped in
/// the source's transform, before the instance sees it, so it settles nothing
/// (see the decode-failure paragraph on `Saga::handle`).
#[tokio::test]
async fn a_corrupt_record_redelivered_over_a_global_cursor_writes_nothing() {
    let ps = ProcessSystem::new().await;

    let upstream_store = Journal::from(InMemoryEventStore::default());
    let source = AggregateId::new("decode-skip-order");
    append_corrupt_order_placed(&upstream_store, &source, 1).await;
    append_order_placed(&upstream_store, &source, 2, "SKU-2").await;

    let saga_store = Journal::from(InMemoryEventStore::default());
    let saga_id = SagaId::new(SAGA_ID);
    // A position row already naming the corrupt record's upstream position, so
    // the count below distinguishes "no row was added for it" from "no row
    // mentions it at all".
    seed_consumed_position(&saga_store, &saga_id, 1, &source, 1).await;

    let handled: Handled = Arc::new(Mutex::new(Vec::new()));
    let _proxy = spawn_deciding(
        &ps,
        &saga_store,
        from_every_source(&upstream_store),
        &handled,
    )
    .await;

    // Seq 2 is the barrier: the poller delivers ascending and `ask`s one
    // record at a time, so once it has been handled the corrupt seq 1's fate
    // is already decided.
    let barrier = wait_for_handled(&handled, 1).await;
    let events = load_saga_events(&saga_store, &saga_id).await;

    assert_eq!(
        barrier.iter().map(|s| s.sequence).collect::<Vec<_>>(),
        vec![2],
        "the corrupt seq 1 carries no typed event and must never reach handle; \
         stream held {:?}",
        describe(&events)
    );
    let decode_position_rows = positions(&events)
        .into_iter()
        .filter(|p| p.source == source.as_str() && p.sequence == 1)
        .count();
    assert_eq!(
        decode_position_rows,
        1,
        "the corrupt record's position must appear exactly once — the seeded \
         row — and gain no second position-only row from the skipped \
         redelivery; stream held {:?}",
        describe(&events)
    );
}

/// Given a running saga that has just persisted the decision for seq 1,
/// When the same record is delivered again without a restart,
/// Then it is dropped: the position the interpreter advanced in memory answers
/// for the running incarnation too, not only for what replay restored.
#[tokio::test]
async fn the_same_input_delivered_twice_in_one_incarnation_reaches_handle_once() {
    let ps = ProcessSystem::new().await;

    let inner = InMemoryEventStore::default();
    let upstream_store = Journal::from(DuplicatingStore { inner });
    let source = AggregateId::new("duplicate-order");
    append_order_placed(&upstream_store, &source, 1, "SKU-1").await;

    let saga_store = Journal::from(InMemoryEventStore::default());
    let handled: Handled = Arc::new(Mutex::new(Vec::new()));
    let saga_id = SagaId::new(SAGA_ID);

    let _proxy = spawn_deciding(
        &ps,
        &saga_store,
        from_source(&upstream_store, &source),
        &handled,
    )
    .await;

    wait_for_saga_rows(&saga_store, &saga_id, CONSUMED_MARKER, 1).await;
    append_order_placed(&upstream_store, &source, 2, "SKU-2").await;

    let barrier = wait_for_handled(&handled, 2).await;
    let events = load_saga_events(&saga_store, &saga_id).await;

    assert_eq!(
        barrier.iter().map(|s| s.sequence).collect::<Vec<_>>(),
        vec![1, 2],
        "each upstream record was delivered twice; handle must see each once; \
         stream held {:?}",
        describe(&events)
    );
    assert_eq!(
        decisions(&events).len(),
        2,
        "two inputs, two decisions, however often they were delivered; stream \
         held {:?}",
        describe(&events)
    );
}

// CP-FOLD — the position is rebuilt from the journal alone

/// Given a saga stream carrying nothing but a consumed position,
/// When the saga starts,
/// Then the fold alone tells it which upstream records it has already
/// consumed — no cursor, no in-memory state, no prior incarnation.
#[tokio::test]
async fn replay_restores_the_consumed_position_from_the_journal_alone() {
    let ps = ProcessSystem::new().await;

    let upstream_store = Journal::from(InMemoryEventStore::default());
    let source = AggregateId::new("fold-order");
    append_order_placed(&upstream_store, &source, 1, "SKU-1").await;
    append_order_placed(&upstream_store, &source, 2, "SKU-2").await;

    let saga_store = Journal::from(InMemoryEventStore::default());
    let saga_id = SagaId::new(SAGA_ID);
    seed_consumed_position(&saga_store, &saga_id, 1, &source, 1).await;

    let handled: Handled = Arc::new(Mutex::new(Vec::new()));
    let _proxy = spawn_deciding(
        &ps,
        &saga_store,
        from_every_source(&upstream_store),
        &handled,
    )
    .await;

    let barrier = wait_for_handled(&handled, 1).await;
    let events = load_saga_events(&saga_store, &saga_id).await;

    assert_eq!(
        barrier.iter().map(|s| s.sequence).collect::<Vec<_>>(),
        vec![2],
        "seq 1 is recorded as consumed and must be dropped; seq 2 is not and \
         must be handled; stream held {:?}",
        describe(&events)
    );
}

/// Given two positions for one source, the later one lower than the earlier,
/// When the journal is folded,
/// Then the highest wins: a position moves forward only, or a stale row would
/// re-open inputs whose decisions are already durable.
#[tokio::test]
async fn a_later_lower_position_does_not_move_the_position_backwards() {
    let ps = ProcessSystem::new().await;

    let upstream_store = Journal::from(InMemoryEventStore::default());
    let source = AggregateId::new("fold-max-order");
    for (sequence, sku) in [(1u64, "SKU-1"), (2, "SKU-2"), (3, "SKU-3")] {
        append_order_placed(&upstream_store, &source, sequence, sku).await;
    }

    let saga_store = Journal::from(InMemoryEventStore::default());
    let saga_id = SagaId::new(SAGA_ID);
    seed_consumed_position(&saga_store, &saga_id, 1, &source, 2).await;
    seed_consumed_position(&saga_store, &saga_id, 2, &source, 1).await;

    let handled: Handled = Arc::new(Mutex::new(Vec::new()));
    let _proxy = spawn_deciding(
        &ps,
        &saga_store,
        from_every_source(&upstream_store),
        &handled,
    )
    .await;

    let barrier = wait_for_handled(&handled, 1).await;
    let events = load_saga_events(&saga_store, &saga_id).await;

    assert_eq!(
        barrier.iter().map(|s| s.sequence).collect::<Vec<_>>(),
        vec![3],
        "a fold that kept the last row it saw would re-open seq 2; only seq 3 is \
         genuinely unconsumed; stream held {:?}",
        describe(&events)
    );
}

// CP-PER-SOURCE — positions belong to the source they were read from

/// Given a position recorded for upstream stream A,
/// When a record of upstream stream B at the same sequence arrives,
/// Then it reaches `handle`: a position covers the stream it was read from and
/// nothing else, which is what a subscription over several sources needs.
#[tokio::test]
async fn a_position_of_one_source_does_not_cover_another() {
    let ps = ProcessSystem::new().await;

    let upstream_store = Journal::from(InMemoryEventStore::default());
    let source_a = AggregateId::new("per-source-a");
    let source_b = AggregateId::new("per-source-b");
    append_order_placed(&upstream_store, &source_a, 1, "SKU-A1").await;
    append_order_placed(&upstream_store, &source_b, 1, "SKU-B1").await;

    let saga_store = Journal::from(InMemoryEventStore::default());
    let saga_id = SagaId::new(SAGA_ID);
    seed_consumed_position(&saga_store, &saga_id, 1, &source_a, 1).await;

    let handled: Handled = Arc::new(Mutex::new(Vec::new()));
    let _proxy = spawn_deciding(
        &ps,
        &saga_store,
        from_every_source(&upstream_store),
        &handled,
    )
    .await;

    let barrier = wait_for_handled(&handled, 1).await;
    let events = load_saga_events(&saga_store, &saga_id).await;

    assert_eq!(
        barrier,
        vec![Seen {
            source: source_b.as_str().to_owned(),
            sequence: 1,
        }],
        "A/1 is consumed and B/1 is not; a single position shared by every \
         source would silently swallow B/1; stream held {:?}",
        describe(&events)
    );
}

// CP-RESUME-BOUND — where a restarted source's cursor starts from
//
// A host's shared cursor is spoken for by no single instance's consumed
// position, so what it is configured with is where it starts — the sweep from
// there is what activates the instances that are not resident, and each
// instance drops what its own stream already names as consumed.  The two tests
// below pin the two halves of that: the configured position is honoured
// exactly, and it is never undercut by what the journal happens to hold.

/// Given a subscription configured to start past a record the saga has *not*
/// consumed,
/// When a fresh host is spawned over a journal with a lower consumed position,
/// Then the configured floor still holds: `with_after` says "do not deliver
/// these", and a consumed position is a second reason to skip, never a reason
/// to start delivering.
#[tokio::test]
async fn the_configured_floor_survives_a_lower_consumed_position() {
    let ps = ProcessSystem::new().await;

    let upstream_store = Journal::from(InMemoryEventStore::default());
    let source = AggregateId::new("floor-order");
    append_order_placed(&upstream_store, &source, 1, "SKU-1").await;

    let saga_store = Journal::from(InMemoryEventStore::default());
    let handled: Handled = Arc::new(Mutex::new(Vec::new()));
    let saga_id = SagaId::new(SAGA_ID);

    let first = spawn_deciding(
        &ps,
        &saga_store,
        from_source(&upstream_store, &source),
        &handled,
    )
    .await;
    wait_for_saga_rows(&saga_store, &saga_id, CONSUMED_MARKER, 1).await;
    first.stop().await.expect("stop must succeed");
    drop(first);

    for (sequence, sku) in [(2u64, "SKU-2"), (3, "SKU-3"), (4, "SKU-4")] {
        append_order_placed(&upstream_store, &source, sequence, sku).await;
    }

    // Consumed is 1; the operator asks for records after 3.
    let _second = spawn_deciding(
        &ps,
        &saga_store,
        from_source(&upstream_store, &source).with_after(3),
        &handled,
    )
    .await;

    let barrier = wait_for_handled(&handled, 2).await;
    let events = load_saga_events(&saga_store, &saga_id).await;

    assert_eq!(
        barrier.iter().map(|s| s.sequence).collect::<Vec<_>>(),
        vec![1, 4],
        "the configured position must not be undercut by what the journal \
         holds: seq 2 and 3 are below the floor and must never be delivered, \
         however low the consumed position is; stream held {:?}",
        describe(&events)
    );
}

/// Given a `Global` subscription, whose position is a global ordering that a
/// per-source stream sequence cannot be translated into,
/// When a fresh host is spawned over a journal that already holds a position,
/// Then the cursor is left exactly where it was configured — de-duplication is
/// the position's job, and a cursor that tried to read a global position out of
/// a per-source one would skip records nobody consumed.
#[tokio::test]
async fn a_global_subscription_keeps_its_configured_cursor() {
    let ps = ProcessSystem::new().await;

    let queries: Queries = Arc::new(Mutex::new(Vec::new()));
    let upstream_store = Journal::from(QueryRecordingStore {
        inner: InMemoryEventStore::default(),
        queries: Arc::clone(&queries),
    });
    let source = AggregateId::new("global-resume-order");
    append_order_placed(&upstream_store, &source, 1, "SKU-1").await;

    let saga_store = Journal::from(InMemoryEventStore::default());
    let handled: Handled = Arc::new(Mutex::new(Vec::new()));
    let saga_id = SagaId::new(SAGA_ID);

    let first = spawn_deciding(
        &ps,
        &saga_store,
        from_every_source(&upstream_store),
        &handled,
    )
    .await;
    wait_for_saga_rows(&saga_store, &saga_id, CONSUMED_MARKER, 1).await;
    first.stop().await.expect("stop must succeed");
    drop(first);

    take_queries(&queries);

    let _second = spawn_deciding(
        &ps,
        &saga_store,
        from_every_source(&upstream_store),
        &handled,
    )
    .await;
    append_order_placed(&upstream_store, &source, 2, "SKU-2").await;

    let barrier = wait_for_handled(&handled, 2).await;
    let asked_from: Vec<Option<u64>> = take_queries(&queries)
        .iter()
        .map(|q| q.from_global_sequence)
        .collect();

    assert_eq!(
        asked_from.first().copied(),
        Some(Some(1)),
        "the first poll of the restarted Global subscription must start where it \
         was configured to; asked from {asked_from:?}"
    );
    assert!(
        asked_from.iter().all(Option::is_some),
        "a Global subscription polls by global sequence, so no poll may fall \
         back to another dimension; asked from {asked_from:?}"
    );
    assert_eq!(
        barrier.iter().map(|s| s.sequence).collect::<Vec<_>>(),
        vec![1, 2],
        "re-reading the consumed record is expected here; dropping it before \
         handle is what keeps the decision from being made twice"
    );
}

// CP-MULTI-APPEND — one handle call, several appends, one position

/// Given one `handle` whose effect reaches the store as two appends — a
/// `CancelSchedule` is a boundary the `Persist × Persist` fold does not cross,
/// When both land,
/// Then the input is recorded as consumed once: the position rides the last
/// batch to land, never the first, so it cannot become durable while a
/// decision this same input produced is still not.  One input was read, so
/// the stream says one input was consumed.
#[tokio::test]
async fn an_input_whose_effect_takes_two_appends_is_recorded_as_consumed_once() {
    let ps = ProcessSystem::new().await;

    let upstream_store = Journal::from(InMemoryEventStore::default());
    let source = AggregateId::new("two-batch-order");
    append_order_placed(&upstream_store, &source, 1, "SKU-1").await;

    let saga_store = Journal::from(InMemoryEventStore::default());
    let handled: Handled = Arc::new(Mutex::new(Vec::new()));
    let saga_id = SagaId::new(SAGA_ID);

    let _proxy = spawn_two_batch(
        &ps,
        &saga_store,
        from_source(&upstream_store, &source),
        &handled,
    )
    .await;

    // The second decision is the barrier: it belongs to the later append, so
    // once it is on the stream both batches are over.
    let events =
        wait_for_saga_rows(&saga_store, &saga_id, ReservationRequested::EVENT_TYPE, 2).await;

    assert_eq!(
        seen(&handled).len(),
        1,
        "one upstream record must reach handle once; stream held {:?}",
        describe(&events)
    );
    assert_eq!(
        positions(&events),
        vec![consumed(&source, 1)],
        "the position is committed with the last batch, once every decision \
         this input produced is durable, so the first batch must carry none; \
         stream held {:?}",
        describe(&events)
    );
}

/// Given one `handle` whose effect is `Persist.then_end().combine(Persist)` —
/// `End` sits between two `Persist` branches, so the physically last element
/// of the `Sequence` is never interpreted,
/// When the effect is interpreted,
/// Then the position rides the `Ended` append — the one that actually closes
/// the input out — and not the unreachable trailing `Persist`; the input is
/// recorded as consumed even though the append the vector's tail sits on
/// never runs.
#[tokio::test]
async fn an_input_closed_out_by_end_mid_sequence_is_recorded_as_consumed() {
    let ps = ProcessSystem::new().await;

    let upstream_store = Journal::from(InMemoryEventStore::default());
    let source = AggregateId::new("persist-then-end-order");
    append_order_placed(&upstream_store, &source, 1, "SKU-1").await;

    let saga_store = Journal::from(InMemoryEventStore::default());
    let handled: Handled = Arc::new(Mutex::new(Vec::new()));
    let saga_id = SagaId::new(SAGA_ID);

    let _proxy = spawn_persist_then_end(
        &ps,
        &saga_store,
        from_source(&upstream_store, &source),
        &handled,
    )
    .await;

    // The consumed position is the barrier: it only ever lands on the `Ended`
    // append, so waiting for it also waits for `End` to have been
    // interpreted.
    let events = wait_for_saga_rows(&saga_store, &saga_id, CONSUMED_MARKER, 1).await;

    assert_eq!(
        seen(&handled).len(),
        1,
        "one upstream record must reach handle once; stream held {:?}",
        describe(&events)
    );
    assert_eq!(
        decisions(&events).len(),
        1,
        "the trailing Persist after End must never be interpreted; stream held \
         {:?}",
        describe(&events)
    );
    assert_eq!(
        positions(&events),
        vec![consumed(&source, 1)],
        "the position must be durable even though it rides the `Ended` append \
         rather than the vector's physically last element; stream held {:?}",
        describe(&events)
    );
}

/// Given one `handle` whose effect reaches the store as two appends, the
/// opening one landing but the closing one — the one the position would ride —
/// exhausting its retries before the saga stops,
/// When the saga restarts and upstream redelivers the same input,
/// Then `handle` runs again and this time both appends land: the decision the
/// first attempt could not durably record is not lost, and the position is
/// finally committed once, by the append that closes the input out.
#[tokio::test]
async fn a_closing_batch_that_never_landed_is_recovered_by_redelivery() {
    let ps = ProcessSystem::new().await;

    let upstream_store = Journal::from(InMemoryEventStore::default());
    let source = AggregateId::new("two-batch-crash-order");
    append_order_placed(&upstream_store, &source, 1, "SKU-1").await;

    // Only the closing append of a `Sequence` carries a position, so a budget
    // spent on position-carrying appends refuses exactly that one, for exactly
    // one batch's staged retries.
    let saga_store = FailFirstAppendsStore::refusing(carries_a_position, STAGED_RETRIES);
    let handled: Handled = Arc::new(Mutex::new(Vec::new()));
    let saga_id = SagaId::new(SAGA_ID);

    let first = spawn_two_batch(
        &ps,
        &saga_store,
        from_source(&upstream_store, &source),
        &handled,
    )
    .await;

    wait_for_handled(&handled, 1).await;
    stop_and_wait(&ps, &first).await;
    drop(first);

    let events = load_saga_events(&saga_store, &saga_id).await;
    assert_eq!(
        decisions(&events).len(),
        1,
        "the opening batch must have landed before the closing one started \
         failing; stream held {:?}",
        describe(&events)
    );
    assert!(
        positions(&events).is_empty(),
        "the closing batch never landed, so no position may be on the stream; \
         stream held {:?}",
        describe(&events)
    );

    let _second = spawn_two_batch(
        &ps,
        &saga_store,
        from_source(&upstream_store, &source),
        &handled,
    )
    .await;

    let events = wait_for_saga_rows(&saga_store, &saga_id, CONSUMED_MARKER, 1).await;

    assert_eq!(
        times_handled(&handled, &source, 1),
        2,
        "no position was durable after the first attempt, so redelivery must \
         reach handle again; invocations: {:?}",
        seen(&handled)
    );
    assert_eq!(
        decisions(&events).len(),
        3,
        "the opening decision from the first attempt plus both decisions from \
         the successful second attempt must all be on the stream — the \
         closing decision the first attempt could not durably record is not \
         lost; stream held {:?}",
        describe(&events)
    );
    assert_eq!(
        positions(&events),
        vec![consumed(&source, 1)],
        "the position is committed exactly once, by the batch that finally \
         closes the input out; stream held {:?}",
        describe(&events)
    );
}

/// Given one `handle` whose effect reaches the store as two appends, the
/// opening one exhausting its retries while the closing one lands anyway,
/// When the saga restarts and upstream redelivers the same input,
/// Then nothing on the stream says the input was consumed and `handle` runs
/// again: an input is consumed only once *every* append it required has landed,
/// so the decision the opening batch lost is re-made rather than skipped
/// forever by a position the closing batch had already committed.
#[tokio::test]
async fn an_input_whose_opening_batch_never_landed_is_not_recorded_as_consumed() {
    let ps = ProcessSystem::new().await;

    let upstream_store = Journal::from(InMemoryEventStore::default());
    let source = AggregateId::new("two-batch-opening-crash-order");
    append_order_placed(&upstream_store, &source, 1, "SKU-1").await;

    // The opening `Persist` is the first append carrying a decision, and its
    // staged retries spend the whole budget — so the closing one, which the
    // position would have ridden, is accepted.  That is the one shape where a
    // position could outlive the decision it was supposed to be as durable as.
    let saga_store = FailFirstAppendsStore::refusing(carries_a_decision, STAGED_RETRIES);
    let handled: Handled = Arc::new(Mutex::new(Vec::new()));
    let saga_id = SagaId::new(SAGA_ID);

    let first = spawn_two_batch(
        &ps,
        &saga_store,
        from_source(&upstream_store, &source),
        &handled,
    )
    .await;

    wait_for_handled(&handled, 1).await;
    stop_and_wait(&ps, &first).await;
    drop(first);

    let events = load_saga_events(&saga_store, &saga_id).await;
    assert_eq!(
        decisions(&events).len(),
        1,
        "only the closing batch landed: the opening one this same input produced \
         exhausted its retries; stream held {:?}",
        describe(&events)
    );
    assert!(
        positions(&events).is_empty(),
        "an append this input required never landed, so the input is not \
         consumed and no position may be on the stream — one committed here \
         would be durable while a decision it stands for is not; stream held \
         {:?}",
        describe(&events)
    );

    let _second = spawn_two_batch(
        &ps,
        &saga_store,
        from_source(&upstream_store, &source),
        &handled,
    )
    .await;

    let events = wait_for_saga_rows(&saga_store, &saga_id, CONSUMED_MARKER, 1).await;

    assert_eq!(
        times_handled(&handled, &source, 1),
        2,
        "no position was durable after the first attempt, so redelivery must \
         reach handle again; invocations: {:?}",
        seen(&handled)
    );
    assert_eq!(
        decisions(&events).len(),
        3,
        "the closing decision the first attempt managed plus both decisions the \
         second attempt persisted — the opening decision the first attempt lost \
         is re-made, not skipped; stream held {:?}",
        describe(&events)
    );
    assert_eq!(
        positions(&events),
        vec![consumed(&source, 1)],
        "the position is committed exactly once, by the attempt that landed \
         every append the input required; stream held {:?}",
        describe(&events)
    );
}

/// Given one `handle` whose effect reaches the store as two appends — an
/// opening `Persist` and a closing `End` — the opening one exhausting its
/// retries before the closing append is ever attempted,
/// When the closing `End` would otherwise run anyway,
/// Then no durable `Ended` fact is written for it: closing the input out here
/// would let the revival guard refuse every future spawn, permanently losing
/// the opening decision that never landed.  Only after the saga restarts and
/// upstream redelivers the same input does `handle` run again, this time
/// landing both appends and finally terminating the saga with the position
/// riding the same append as `Ended`.
#[tokio::test]
async fn an_input_whose_opening_batch_never_landed_leaves_a_closing_end_unwritten() {
    let ps = ProcessSystem::new().await;

    let upstream_store = Journal::from(InMemoryEventStore::default());
    let source = AggregateId::new("opening-crash-then-end-order");
    append_order_placed(&upstream_store, &source, 1, "SKU-1").await;

    // Only the opening `Persist` carries a decision; the closing `End` does
    // not, so the store would have accepted that append had the interpreter
    // attempted it.  Whether it did is exactly what is under test.
    let saga_store = FailFirstAppendsStore::refusing(carries_a_decision, STAGED_RETRIES);
    let handled: Handled = Arc::new(Mutex::new(Vec::new()));
    let saga_id = SagaId::new(SAGA_ID);

    let first = spawn_persist_then_end(
        &ps,
        &saga_store,
        from_source(&upstream_store, &source),
        &handled,
    )
    .await;

    wait_for_handled(&handled, 1).await;
    stop_and_wait(&ps, &first).await;
    drop(first);

    let events = load_saga_events(&saga_store, &saga_id).await;
    assert!(
        decisions(&events).is_empty(),
        "the opening batch never landed, so no decision may be on the stream; \
         stream held {:?}",
        describe(&events)
    );
    assert!(
        !events
            .iter()
            .any(|e| matches!(lifecycle_kind_of(e), Some(LifecycleKind::Ended(_)))),
        "a preceding append that never landed must withhold the closing `End` \
         fact too — durably ending the saga here would refuse every future \
         revival and lose the opening decision for good; stream held {:?}",
        describe(&events)
    );
    assert!(
        positions(&events).is_empty(),
        "an append this input required never landed, so the input is not \
         consumed and no position may be on the stream; stream held {:?}",
        describe(&events)
    );

    let _second = spawn_persist_then_end(
        &ps,
        &saga_store,
        from_source(&upstream_store, &source),
        &handled,
    )
    .await;

    let events = wait_for_saga_rows(&saga_store, &saga_id, CONSUMED_MARKER, 1).await;

    assert_eq!(
        times_handled(&handled, &source, 1),
        2,
        "no position was durable after the first attempt, so redelivery must \
         reach handle again; invocations: {:?}",
        seen(&handled)
    );
    assert_eq!(
        decisions(&events).len(),
        1,
        "the opening decision the first attempt lost is re-made by the \
         second, successful attempt — the trailing Persist after End is still \
         never interpreted; stream held {:?}",
        describe(&events)
    );
    assert_eq!(
        events
            .iter()
            .filter_map(lifecycle_kind_of)
            .filter(|kind| matches!(kind, LifecycleKind::Ended(_)))
            .count(),
        1,
        "the second attempt lands every append this input requires, so `End` \
         finally closes it out — once, not once per attempt; stream held {:?}",
        describe(&events)
    );
    assert_eq!(
        position_rows(&events),
        1,
        "and the position is committed exactly once, by the `Ended` append that \
         finally closes the input out; stream held {:?}",
        describe(&events)
    );
    assert_eq!(
        positions(&events),
        vec![consumed(&source, 1)],
        "the one position names the input the `Ended` append settled; stream \
         held {:?}",
        describe(&events)
    );
}

/// Given one `handle` whose effect reaches the store as three appends, the
/// middle one — the `CancelSchedule` marker — never landing,
/// When the closing append lands anyway,
/// Then nothing on the stream says the input was consumed, and redelivery
/// reaches `handle` again.
///
/// The same invariant as the two tests above reached through a different leaf:
/// a schedule-marker append is attempted once and never retried, so if the
/// interpreter treated its failure as an ordinary "carry on" the closing append
/// would record the input as consumed and the cancellation would be lost for
/// good.
#[tokio::test]
async fn an_input_whose_middle_append_never_landed_is_not_recorded_as_consumed() {
    let ps = ProcessSystem::new().await;

    let upstream_store = Journal::from(InMemoryEventStore::default());
    let source = AggregateId::new("middle-append-crash-order");
    append_order_placed(&upstream_store, &source, 1, "SKU-1").await;

    // One refusal is one attempt's worth here: unlike a persist batch, the
    // cancellation marker's append is never retried.
    let saga_store = FailFirstAppendsStore::refusing(carries_a_schedule_marker, 1);
    let handled: Handled = Arc::new(Mutex::new(Vec::new()));
    let saga_id = SagaId::new(SAGA_ID);

    let first = spawn_two_batch(
        &ps,
        &saga_store,
        from_source(&upstream_store, &source),
        &handled,
    )
    .await;

    wait_for_handled(&handled, 1).await;
    stop_and_wait(&ps, &first).await;
    drop(first);

    let events = load_saga_events(&saga_store, &saga_id).await;
    assert!(
        !events.iter().any(|e| e.event_type == SCHEDULE_MARKER),
        "the middle append must have been the one that failed; stream held {:?}",
        describe(&events)
    );
    assert_eq!(
        decisions(&events).len(),
        2,
        "the `Persist` batches on either side of it landed, so the closing one \
         — the append a position would have ridden — really did succeed; \
         stream held {:?}",
        describe(&events)
    );
    assert!(
        positions(&events).is_empty(),
        "an append this input required never landed, so the input is not \
         consumed and no position may be on the stream; stream held {:?}",
        describe(&events)
    );

    let _second = spawn_two_batch(
        &ps,
        &saga_store,
        from_source(&upstream_store, &source),
        &handled,
    )
    .await;

    let events = wait_for_saga_rows(&saga_store, &saga_id, CONSUMED_MARKER, 1).await;

    assert_eq!(
        times_handled(&handled, &source, 1),
        2,
        "no position was durable after the first attempt, so redelivery must \
         reach handle again; invocations: {:?}",
        seen(&handled)
    );
    assert_eq!(
        events
            .iter()
            .filter(|e| e.event_type == SCHEDULE_MARKER)
            .count(),
        1,
        "the cancellation the first attempt lost is re-made by the second, \
         successful attempt; stream held {:?}",
        describe(&events)
    );
    assert_eq!(
        decisions(&events).len(),
        4,
        "both `Persist` batches of both attempts landed — the middle append is \
         the only one that ever failed, and its failure is what re-ran the \
         input; stream held {:?}",
        describe(&events)
    );
    assert_eq!(
        positions(&events),
        vec![consumed(&source, 1)],
        "the position is committed exactly once, by the attempt that landed \
         every append the input required; stream held {:?}",
        describe(&events)
    );
}

// CP-EMPTY-STILL-CONSUMED — a decision-less input is still consumed

/// Given a `handle` that returns an all-empty `Persist` — no user event or
/// schedule for the interpreter to batch,
/// When the saga restarts and the same input is redelivered,
/// Then it does not reach `handle` again: the interpreter still committed a
/// consumed-position-only append for it, because the input had a position to
/// record even though the decision itself was empty.
///
/// Upstream seq 2 is the barrier: the poller delivers ascending, so once seq 2
/// has been handled a second delivery of seq 1 can no longer be pending.
#[tokio::test]
async fn an_input_that_wrote_nothing_is_still_consumed() {
    let ps = ProcessSystem::new().await;

    let upstream_store = Journal::from(InMemoryEventStore::default());
    let source = AggregateId::new("empty-persist-order");
    append_order_placed(&upstream_store, &source, 1, "SKU-1").await;

    let saga_store = Journal::from(InMemoryEventStore::default());
    let handled: Handled = Arc::new(Mutex::new(Vec::new()));
    let saga_id = SagaId::new(SAGA_ID);

    let handled_for_first = Arc::clone(&handled);
    let first =
        SagaManagerProps::<EmptyPersistSaga>::new(saga_store.clone(), move |_| EmptyPersistSaga {
            handled: Arc::clone(&handled_for_first),
        })
        .observing::<OrderPlaced>(from_source(&upstream_store, &source))
        .spawn(&ps)
        .await;

    wait_for_saga_rows(&saga_store, &saga_id, CONSUMED_MARKER, 1).await;
    first.stop().await.expect("stop must succeed");
    drop(first);

    let handled_for_second = Arc::clone(&handled);
    let _second =
        SagaManagerProps::<EmptyPersistSaga>::new(saga_store.clone(), move |_| EmptyPersistSaga {
            handled: Arc::clone(&handled_for_second),
        })
        .observing::<OrderPlaced>(from_source(&upstream_store, &source))
        .spawn(&ps)
        .await;
    append_order_placed(&upstream_store, &source, 2, "SKU-2").await;

    let barrier = wait_for_handled(&handled, 2).await;
    let events = load_saga_events(&saga_store, &saga_id).await;

    assert_eq!(
        barrier.iter().map(|s| s.sequence).collect::<Vec<_>>(),
        vec![1, 2],
        "seq 1 belongs to the first incarnation alone; the second must see only \
         the input it has not consumed; stream held {:?}",
        describe(&events)
    );
    assert_eq!(
        decisions(&events).len(),
        0,
        "this saga never persists a decision for either input; stream held {:?}",
        describe(&events)
    );
    assert_eq!(
        positions(&events),
        vec![consumed(&source, 1), consumed(&source, 2)],
        "a position is recorded for seq 1 even though handle decided nothing \
         for it, and again for seq 2; stream held {:?}",
        describe(&events)
    );
}

// CP-NO-INPUT-NO-POSITION — an entry point with no input commits no position

/// Given a saga that has consumed upstream seq 1 and set a deadline from it,
/// When the deadline fires and its arm of `handle` persists a decision of its
/// own,
/// Then the append that decision reached the store in carries no position row:
/// a deadline firing was decided from no upstream record, so there is no
/// position it could honestly claim.
///
/// Why this matters beyond a redundant row: a position is what the framework
/// skips redelivery on. A position written for a record that was never
/// delivered would make the saga skip a genuinely unconsumed upstream record —
/// silent event loss, and the one failure the per-marker-type counts in
/// `saga_deadline_scheduler.rs` cannot see.
///
/// The deadline's own decision landing is the barrier: it is the only
/// `ReservationRequested` this saga ever writes, so it cannot be on the stream
/// before the deadline arm ran and its append committed.
#[tokio::test]
async fn a_timer_firing_writes_no_consumed_position() {
    let ps = ProcessSystem::new().await;

    let upstream_store = Journal::from(InMemoryEventStore::default());
    let source = AggregateId::new("timer-order");
    append_order_placed(&upstream_store, &source, 1, "SKU-1").await;

    let batches: Batches = Arc::new(Mutex::new(Vec::new()));
    let saga_store = Journal::from(RecordingStore {
        inner: InMemoryEventStore::default(),
        batches: Arc::clone(&batches),
    });
    let fired = Arc::new(AtomicU64::new(0));
    let saga_id = SagaId::new(SAGA_ID);

    let fired_for_producer = Arc::clone(&fired);
    let _proxy = SagaManagerProps::<SchedulingSaga>::new(saga_store.clone(), move |components| {
        SchedulingSaga {
            deadline: components.deadline(),
            fired: Arc::clone(&fired_for_producer),
        }
    })
    // No scheduler wiring: the alphabet declaring a deadline is the whole
    // declaration, and the host resolves the resident timer core from it.
    .observing::<OrderPlaced>(from_source(&upstream_store, &source))
    .spawn(&ps)
    .await;

    let events =
        wait_for_saga_rows(&saga_store, &saga_id, ReservationRequested::EVENT_TYPE, 1).await;

    assert!(
        fired.load(Ordering::SeqCst) >= 1,
        "the scheduled timer must have fired for this test to observe the \
         append it produced; stream held {:?}",
        describe(&events)
    );
    assert_eq!(
        positions(&events),
        vec![consumed(&source, 1)],
        "the only position on the stream is the upstream input's — the timer \
         firing added none; stream held {:?}",
        describe(&events)
    );
    assert_eq!(
        position_rows(&events),
        1,
        "and nothing unreadable was written under the position path either; \
         stream held {:?}",
        describe(&events)
    );

    let recorded = batches
        .lock()
        .expect("recorded batches must not be poisoned")
        .clone();
    let timer_batches: Vec<&Vec<RecordedEvent>> = recorded
        .iter()
        .filter(|batch| {
            batch
                .iter()
                .any(|e| e.event_type == ReservationRequested::EVENT_TYPE)
        })
        .collect();
    assert!(
        !timer_batches.is_empty(),
        "the timer's decision must have been recorded as an append for this \
         test to say anything about it; stream held {:?}",
        describe(&events)
    );
    for batch in &timer_batches {
        let rows: Vec<String> = batch
            .iter()
            .map(|e| format!("{}#{}", e.sequence, e.event_type))
            .collect();
        assert_eq!(
            batch
                .iter()
                .filter(|e| e.event_type == CONSUMED_MARKER)
                .count(),
            0,
            "the append a timer firing produced carries no position row; batch \
             held {rows:?}"
        );
    }
}
