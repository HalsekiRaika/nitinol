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
//!     When the closing append lands and the saga restarts
//!     Then no position is on the stream and handle runs again
//!
//!   Scenario: a crash before the append is re-handled
//!     Given the batch for upstream seq N never landed
//!     When upstream redelivers seq N after a restart
//!     Then handle runs again (at-least-once; no position was written)
//!
//!   Scenario: a subscription resumes past the folded position
//!     Given positions up to upstream seq N are on the saga's stream
//!     When the saga restarts and resubscribes
//!     Then the subscription asks upstream for records after seq N
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
//! it is stored under, and the `oneof` its payload carries.
//!
//! Negative claims ("this never reached handle") are never settled by a sleep:
//! each uses a later upstream record as a barrier.  The poller delivers a
//! stream in ascending order and `ask`s one record at a time, so once the
//! barrier has been handled the earlier record's fate is already decided.

#[path = "common/helpers.rs"]
mod common;
use common::{
    dead_letter_of, encode_consumed_position, encode_outbox_tell_failed,
    encode_outbox_tell_requested_with_target, failure_kind, outbox_kind_of,
    readable_consumed_position_of, ConsumedPayload, JsonCodec, OutboxKind,
    CONSUMED_POSITION_MARKER, DEAD_LETTER_MARKER, OUTBOX_MARKER, SCHEDULE_MARKER,
};

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use futures_util::TryStreamExt;
use serde::{Deserialize, Serialize};

use nitinol_eventsource::codec::Codec;
use nitinol_eventsource::{
    system::EventSourceSystem, Aggregate, AggregateProxy, Decider, Decision, Event,
};
use nitinol_persistence::error::{AppendError, LoadError};
use nitinol_persistence::store::{EventStore, EventStream, InMemoryEventStore};
use nitinol_persistence::{
    AggregateId, AppendOutcome, AppendingEvent, EventType, Family, LoadQuery, LoadedEvent, TypeName,
};
use nitinol_runtime::ProcessSystem;
use nitinol_saga::{
    Deadline, DeadlineMessage, Saga, SagaContext, SagaEffect, SagaId, SagaProps, SagaSubscribe,
    Subscription, TimerName,
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

#[derive(Clone, Debug, Serialize, Deserialize)]
struct InventoryReserved {
    sku: String,
}

impl Event for InventoryReserved {
    const EVENT_TYPE: EventType = EventType::new(
        Family::new("consumed_position"),
        TypeName::new("InventoryReserved"),
    );
    type Codec = JsonCodec;
}

#[derive(Default)]
struct Inventory;

impl Aggregate for Inventory {
    type Event = InventoryReserved;
    fn apply(&mut self, _event: InventoryReserved) {}
}

#[derive(Clone, Serialize, Deserialize)]
struct Reserve {
    sku: String,
}

impl Decider<Reserve> for Inventory {
    type Output = ();
    type Rejection = std::convert::Infallible;

    fn decide(&self, cmd: Reserve) -> Decision<InventoryReserved, (), Self::Rejection> {
        Decision::persist(vec![InventoryReserved { sku: cmd.sku }]).output(())
    }
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
    failed_tell_ids: Vec<u64>,
}

type Handled = Arc<Mutex<Vec<Seen>>>;

fn record(handled: &Handled, ctx: &SagaContext) {
    handled
        .lock()
        .expect("handled buffer must not be poisoned")
        .push(Seen {
            source: ctx.upstream_aggregate_id().as_str().to_owned(),
            sequence: ctx.upstream_sequence(),
            failed_tell_ids: ctx.failed_tell_ids().to_vec(),
        });
}

fn seen(handled: &Handled) -> Vec<Seen> {
    handled
        .lock()
        .expect("handled buffer must not be poisoned")
        .clone()
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

/// Persists a decision *and* tells an aggregate, so one batch carries a user
/// event, a `TellRequested` marker and the consumed position together.
struct DecidingAndTellingSaga {
    handled: Handled,
    inventory: AggregateProxy<Inventory>,
}

#[async_trait]
impl Saga for DecidingAndTellingSaga {
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
        .combine(SagaEffect::tell(
            self.inventory.clone(),
            Reserve { sku: event.sku },
        )))
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

/// Rejects any append carrying the saga's own decision event while `failing`
/// is set, and accepts everything else.
///
/// Modelling a crash *before* the batch lands rather than a store that is
/// wholly down: the dead letter reporting the failure still has to be
/// writable, or the saga would refuse the upstream record instead of moving
/// on, and the "no position was written" claim would be vacuous.
struct FailDecisionAppendStore {
    inner: InMemoryEventStore,
    failing: Arc<AtomicBool>,
}

#[async_trait]
impl EventStore for FailDecisionAppendStore {
    async fn append(
        &self,
        key: &str,
        events: Vec<AppendingEvent>,
    ) -> Result<AppendOutcome, AppendError> {
        if self.failing.load(Ordering::SeqCst)
            && events
                .iter()
                .any(|e| e.event_type == ReservationRequested::EVENT_TYPE)
        {
            return Err(AppendError::Backend(
                "injected: the decision batch never landed".into(),
            ));
        }
        self.inner.append(key, events).await
    }

    async fn load(&self, query: LoadQuery) -> Result<EventStream<'_>, LoadError> {
        self.inner.load(query).await
    }
}

/// Rejects the second append carrying the saga's own decision event while
/// `failing` is set, letting the first one land.
///
/// Models a crash between the two appends a `Sequence` with a hard boundary
/// (`CancelSchedule` or `End`) produces: the opening batch is durable, the
/// closing batch — the one the position would ride — never lands.
struct FailSecondDecisionAppendStore {
    inner: InMemoryEventStore,
    failing: Arc<AtomicBool>,
    decision_appends: Arc<AtomicU64>,
}

#[async_trait]
impl EventStore for FailSecondDecisionAppendStore {
    async fn append(
        &self,
        key: &str,
        events: Vec<AppendingEvent>,
    ) -> Result<AppendOutcome, AppendError> {
        let carries_decision = events
            .iter()
            .any(|e| e.event_type == ReservationRequested::EVENT_TYPE);
        if carries_decision {
            let seen = self.decision_appends.fetch_add(1, Ordering::SeqCst) + 1;
            if self.failing.load(Ordering::SeqCst) && seen >= 2 {
                return Err(AppendError::Backend(
                    "injected: the closing batch never landed".into(),
                ));
            }
        }
        self.inner.append(key, events).await
    }

    async fn load(&self, query: LoadQuery) -> Result<EventStream<'_>, LoadError> {
        self.inner.load(query).await
    }
}

/// Rejects every append carrying the saga's own decision event until the dead
/// letter reporting that failure has landed, and accepts everything after it.
///
/// Models the *opening* batch of a multi-append interpretation exhausting its
/// retries: because the dead letter is writable, the interpreter treats the
/// input as reportable rather than undeliverable and carries on, so the closing
/// batch — the one the position rides — lands even though a decision this same
/// input produced never did.  That is the one shape where a position could
/// outlive the decision it was supposed to be as durable as.
struct FailUntilReportedStore {
    inner: InMemoryEventStore,
    reported: AtomicBool,
}

#[async_trait]
impl EventStore for FailUntilReportedStore {
    async fn append(
        &self,
        key: &str,
        events: Vec<AppendingEvent>,
    ) -> Result<AppendOutcome, AppendError> {
        let carries_decision = events
            .iter()
            .any(|e| e.event_type == ReservationRequested::EVENT_TYPE);
        if carries_decision && !self.reported.load(Ordering::SeqCst) {
            return Err(AppendError::Backend(
                "injected: the opening batch never landed".into(),
            ));
        }
        let reports_failure = events.iter().any(|e| e.event_type == DEAD_LETTER_MARKER);
        let outcome = self.inner.append(key, events).await?;
        if reports_failure {
            self.reported.store(true, Ordering::SeqCst);
        }
        Ok(outcome)
    }

    async fn load(&self, query: LoadQuery) -> Result<EventStream<'_>, LoadError> {
        self.inner.load(query).await
    }
}

/// Rejects the append carrying a schedule marker while `failing` is set, and
/// accepts everything else.
///
/// Models the *middle* append of a multi-append interpretation failing.  The
/// `Persist` batches on either side of the `CancelSchedule` land, so the
/// closing one — the append the position rides — succeeds even though an append
/// this same input required did not.  Unlike a persist batch, this one is not
/// retried and enqueues no dead letter, so it is the path on which a swallowed
/// failure would be quietest.
struct FailScheduleMarkerAppendStore {
    inner: InMemoryEventStore,
    failing: Arc<AtomicBool>,
}

#[async_trait]
impl EventStore for FailScheduleMarkerAppendStore {
    async fn append(
        &self,
        key: &str,
        events: Vec<AppendingEvent>,
    ) -> Result<AppendOutcome, AppendError> {
        if self.failing.load(Ordering::SeqCst)
            && events.iter().any(|e| e.event_type == SCHEDULE_MARKER)
        {
            return Err(AppendError::Backend(
                "injected: the schedule marker never landed".into(),
            ));
        }
        self.inner.append(key, events).await
    }

    async fn load(&self, query: LoadQuery) -> Result<EventStream<'_>, LoadError> {
        self.inner.load(query).await
    }
}

/// Yields every stored record twice in one load.
///
/// At-least-once delivery lets the same upstream record reach a *running*
/// saga more than once; this reproduces that deterministically, without the
/// restart the other tests use, so the position the interpreter holds in
/// memory is what the second delivery is judged against.
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

async fn append_order_placed(
    store: &Arc<dyn EventStore>,
    source: &AggregateId,
    sequence: u64,
    sku: &str,
) {
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
    store: &Arc<dyn EventStore>,
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

async fn seed_consumed_position(
    store: &Arc<dyn EventStore>,
    saga_id: &SagaId,
    sequence: u64,
    source: &AggregateId,
    upstream_sequence: u64,
) {
    seed_saga_row(
        store,
        saga_id,
        sequence,
        CONSUMED_POSITION_MARKER,
        encode_consumed_position(source.as_str(), upstream_sequence),
    )
    .await;
}

/// Append an event carrying `OrderPlaced::EVENT_TYPE` but an invalid JSON
/// payload, so the codec's `decode` call fails the same way a corrupted wire
/// record would — exercising the `UpstreamMessage::DecodeFailed` path instead
/// of `Decoded`.
async fn append_corrupt_order_placed(
    store: &Arc<dyn EventStore>,
    source: &AggregateId,
    sequence: u64,
) {
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

async fn load_saga_events(store: &Arc<dyn EventStore>, saga_id: &SagaId) -> Vec<LoadedEvent> {
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
///
/// Unreadable rows are left out rather than panicked on: one test seeds such a
/// row deliberately, and [`position_rows`] is what counts what is physically
/// there.
fn positions(events: &[LoadedEvent]) -> Vec<ConsumedPayload> {
    events
        .iter()
        .filter_map(readable_consumed_position_of)
        .collect()
}

/// How many rows the stream carries under the consumed-position path,
/// readable or not.
fn position_rows(events: &[LoadedEvent]) -> usize {
    events
        .iter()
        .filter(|e| e.event_type == CONSUMED_POSITION_MARKER)
        .count()
}

/// How many dead letters of `kind` the stream carries.
fn count_dead_letter_kind(events: &[LoadedEvent], kind: &'static str) -> usize {
    events
        .iter()
        .filter_map(dead_letter_of)
        .filter(|record| failure_kind(&record.failure) == kind)
        .count()
}

fn describe(events: &[LoadedEvent]) -> Vec<String> {
    events
        .iter()
        .map(|e| match readable_consumed_position_of(e) {
            Some(p) => format!(
                "{}#{}: consumed({}, {})",
                e.sequence, e.event_type, p.source, p.sequence
            ),
            None => format!("{}#{}", e.sequence, e.event_type),
        })
        .collect()
}

/// How long a positive barrier is given before the test calls it a failure.
const BARRIER_TIMEOUT: Duration = Duration::from_secs(5);
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

async fn wait_for_saga_rows(
    store: &Arc<dyn EventStore>,
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
    saga_store: &Arc<dyn EventStore>,
    subscription: Subscription,
    handled: &Handled,
) -> nitinol_saga::SagaProxy<DecidingSaga> {
    let handled = Arc::clone(handled);
    SagaProps::<DecidingSaga>::new(
        SagaId::new(SAGA_ID),
        Arc::clone(saga_store),
        move |_components| DecidingSaga {
            handled: Arc::clone(&handled),
        },
    )
    .subscribed_to::<OrderPlaced>(subscription)
    .spawn(ps)
    .await
}

/// The one declared source of every saga in this file, read from the `source`
/// stream of `upstream_store` from the beginning.
fn from_source(upstream_store: &Arc<dyn EventStore>, source: &AggregateId) -> Subscription {
    Subscription::stream(upstream_store, source)
}

/// The same source, read from every stream of `upstream_store` at once in the
/// store's own global order.
///
/// A store-wide position is not a stream position, so it is not carried forward
/// from the consumed positions the saga's own stream records: a source bound
/// this way really does re-read what it has already consumed after a restart,
/// which is what lets the tests below put the framework's skip — rather than
/// the cursor's — under test.
fn from_every_source(upstream_store: &Arc<dyn EventStore>) -> Subscription {
    Subscription::global(upstream_store)
}

// CP-ATOMIC — the decision and the position reach the store as one append

/// Given a saga whose `handle` returns one decision for upstream seq 1,
/// When the framework persists the result,
/// Then a single append carries both the decision and the consumed position:
/// neither is ever in the store without the other.
#[tokio::test]
async fn the_decision_and_the_consumed_position_reach_the_store_in_one_append() {
    let ps = ProcessSystem::new().await;

    let upstream_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let source = AggregateId::new("atomic-order");
    append_order_placed(&upstream_store, &source, 1, "SKU-1").await;

    let batches: Batches = Arc::new(Mutex::new(Vec::new()));
    let saga_store: Arc<dyn EventStore> = Arc::new(RecordingStore {
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
    let events = wait_for_saga_rows(&saga_store, &saga_id, CONSUMED_POSITION_MARKER, 1).await;

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
    let decision_rows = batch
        .iter()
        .filter(|e| e.event_type == ReservationRequested::EVENT_TYPE)
        .count();
    let position_rows: Vec<&RecordedEvent> = batch
        .iter()
        .filter(|e| e.event_type == CONSUMED_POSITION_MARKER)
        .collect();
    assert_eq!(
        decision_rows,
        1,
        "the one append must carry the decision; stream held {:?}",
        describe(&events)
    );
    assert_eq!(
        position_rows.len(),
        1,
        "the one append must carry the position of the input it was decided \
         from; stream held {:?}",
        describe(&events)
    );

    let consumed = common::decode_consumed_position(&position_rows[0].payload);
    assert_eq!(
        consumed,
        ConsumedPayload {
            source: source.as_str().to_owned(),
            sequence: 1,
        },
        "the position names the upstream stream and the sequence handled"
    );

    let stamp = batch[0].occurred_at;
    assert!(
        batch.iter().all(|e| e.occurred_at == stamp),
        "every row of one commit carries the commit's own timestamp; a position \
         stamped from a second clock reading would break the one signal a reader \
         has that the rows landed together"
    );
}

/// Given a batch that also carries a tell,
/// When it is persisted,
/// Then the position is the last row: every `tell_id` still equals the stream
/// sequence of its own `TellRequested` marker, which is the identity the outbox
/// re-dispatch path resolves a pending tell by.
#[tokio::test]
async fn the_position_is_the_last_row_so_no_tell_id_moves() {
    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::builder(ps).build();

    let upstream_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let source = AggregateId::new("tell-order");
    append_order_placed(&upstream_store, &source, 1, "SKU-1").await;

    let inventory_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let inventory = system
        .spawn_aggregate::<Inventory>(AggregateId::new("tell-inventory"), inventory_store)
        .await;

    let batches: Batches = Arc::new(Mutex::new(Vec::new()));
    let saga_store: Arc<dyn EventStore> = Arc::new(RecordingStore {
        inner: InMemoryEventStore::default(),
        batches: Arc::clone(&batches),
    });
    let handled: Handled = Arc::new(Mutex::new(Vec::new()));

    let handled_for_producer = Arc::clone(&handled);
    let _proxy = SagaProps::<DecidingAndTellingSaga>::new(
        SagaId::new(SAGA_ID),
        Arc::clone(&saga_store),
        move |_components| DecidingAndTellingSaga {
            handled: Arc::clone(&handled_for_producer),
            inventory: inventory.clone(),
        },
    )
    .subscribed_to::<OrderPlaced>(from_source(&upstream_store, &source))
    .spawn(system.process_system())
    .await;

    let saga_id = SagaId::new(SAGA_ID);
    let events = wait_for_saga_rows(&saga_store, &saga_id, CONSUMED_POSITION_MARKER, 1).await;

    let recorded = batches
        .lock()
        .expect("recorded batches must not be poisoned")
        .clone();
    let atomic_batch = recorded
        .iter()
        .find(|batch| {
            batch
                .iter()
                .any(|e| e.event_type == ReservationRequested::EVENT_TYPE)
        })
        .expect("the decision batch must have been recorded");

    let types: Vec<String> = atomic_batch
        .iter()
        .map(|e| e.event_type.to_string())
        .collect();
    assert_eq!(
        types,
        vec![
            ReservationRequested::EVENT_TYPE.to_string(),
            OUTBOX_MARKER.to_string(),
            CONSUMED_POSITION_MARKER.to_string(),
        ],
        "the position goes last: placed anywhere earlier it would shift every \
         row after it, and a tell_id is the sequence of its own row; stream held \
         {:?}",
        describe(&events)
    );

    let tell_row = &atomic_batch[1];
    let tell_id = match common::decode_outbox_kind(&tell_row.payload) {
        common::OutboxKind::TellRequested(m) => m.tell_id,
        other => panic!("the second row must be a TellRequested marker, got {other:?}"),
    };
    assert_eq!(
        tell_id, tell_row.sequence,
        "a tell is identified by the stream sequence of the marker that requested \
         it; adding a row before it would silently repoint every pending tell"
    );
}

// CP-ATOMIC-FAIL / CP-ATLEASTONCE — a batch that never landed leaves no position

/// Given an append that rejects the decision batch,
/// When the saga handles the upstream record,
/// Then no consumed position is on the saga's stream: a position is only ever
/// as durable as the decision it was committed with.
#[tokio::test]
async fn a_batch_that_never_landed_writes_no_consumed_position() {
    let ps = ProcessSystem::new().await;

    let upstream_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let source = AggregateId::new("failed-batch-order");
    append_order_placed(&upstream_store, &source, 1, "SKU-1").await;

    let failing = Arc::new(AtomicBool::new(true));
    let saga_store: Arc<dyn EventStore> = Arc::new(FailDecisionAppendStore {
        inner: InMemoryEventStore::default(),
        failing: Arc::clone(&failing),
    });
    let handled: Handled = Arc::new(Mutex::new(Vec::new()));

    let _proxy = spawn_deciding(
        &ps,
        &saga_store,
        from_source(&upstream_store, &source),
        &handled,
    )
    .await;

    // The dead letter the exhausted batch enqueues is the barrier: it is
    // written after the last append attempt has failed, so the interpretation
    // of this record is over by the time it appears.
    let saga_id = SagaId::new(SAGA_ID);
    let events = wait_for_saga_rows(&saga_store, &saga_id, DEAD_LETTER_MARKER, 1).await;

    assert_eq!(
        wait_for_handled(&handled, 1).await.len(),
        1,
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
}

/// Given an input whose batch never landed before the saga stopped,
/// When a fresh incarnation is spawned and upstream redelivers it,
/// Then `handle` runs again — at-least-once, because no position was written.
#[tokio::test]
async fn an_input_whose_batch_never_landed_is_handled_again_after_restart() {
    let ps = ProcessSystem::new().await;

    let upstream_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let source = AggregateId::new("at-least-once-order");
    append_order_placed(&upstream_store, &source, 1, "SKU-1").await;

    let failing = Arc::new(AtomicBool::new(true));
    let saga_store: Arc<dyn EventStore> = Arc::new(FailDecisionAppendStore {
        inner: InMemoryEventStore::default(),
        failing: Arc::clone(&failing),
    });
    let handled: Handled = Arc::new(Mutex::new(Vec::new()));
    let saga_id = SagaId::new(SAGA_ID);

    let first = spawn_deciding(
        &ps,
        &saga_store,
        from_source(&upstream_store, &source),
        &handled,
    )
    .await;
    wait_for_saga_rows(&saga_store, &saga_id, DEAD_LETTER_MARKER, 1).await;
    first.stop().await.expect("stop must succeed");
    drop(first);

    // The store recovers; the saga restarts over the same stream.
    failing.store(false, Ordering::SeqCst);

    let _second = spawn_deciding(
        &ps,
        &saga_store,
        from_source(&upstream_store, &source),
        &handled,
    )
    .await;

    let events = wait_for_saga_rows(&saga_store, &saga_id, CONSUMED_POSITION_MARKER, 1).await;

    let invocations = seen(&handled);
    assert_eq!(
        invocations
            .iter()
            .filter(|s| s.source == source.as_str() && s.sequence == 1)
            .count(),
        2,
        "the first attempt wrote no position, so redelivery must reach handle \
         again; invocations: {invocations:?}"
    );
    assert_eq!(
        decisions(&events).len(),
        1,
        "the re-run is what finally persists the decision; stream held {:?}",
        describe(&events)
    );
    assert_eq!(
        positions(&events),
        vec![ConsumedPayload {
            source: source.as_str().to_owned(),
            sequence: 1,
        }],
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

    let upstream_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let source = AggregateId::new("skip-order");
    append_order_placed(&upstream_store, &source, 1, "SKU-1").await;

    let saga_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let handled: Handled = Arc::new(Mutex::new(Vec::new()));
    let saga_id = SagaId::new(SAGA_ID);

    let first = spawn_deciding(
        &ps,
        &saga_store,
        from_source(&upstream_store, &source),
        &handled,
    )
    .await;
    wait_for_saga_rows(&saga_store, &saga_id, CONSUMED_POSITION_MARKER, 1).await;
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

    let upstream_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let source = AggregateId::new("global-skip-order");
    append_order_placed(&upstream_store, &source, 1, "SKU-1").await;

    let saga_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let handled: Handled = Arc::new(Mutex::new(Vec::new()));
    let saga_id = SagaId::new(SAGA_ID);

    let first = spawn_deciding(
        &ps,
        &saga_store,
        from_every_source(&upstream_store),
        &handled,
    )
    .await;
    wait_for_saga_rows(&saga_store, &saga_id, CONSUMED_POSITION_MARKER, 1).await;
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
/// Then the record never reaches `handle`, no `decode_failed` dead letter is
/// appended for it, and the position it sits at is still named by exactly one
/// row: an undecodable record is reported and skipped in the source's transform,
/// before the instance sees it, so it settles nothing (see the decode-failure
/// paragraph on `Saga::handle`).
#[tokio::test]
async fn a_corrupt_record_redelivered_over_a_global_cursor_writes_nothing() {
    let ps = ProcessSystem::new().await;

    let upstream_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let source = AggregateId::new("decode-skip-order");
    append_corrupt_order_placed(&upstream_store, &source, 1).await;
    append_order_placed(&upstream_store, &source, 2, "SKU-2").await;

    let saga_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
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
    assert_eq!(
        count_dead_letter_kind(&events, "decode_failed"),
        0,
        "an undecodable upstream record is reported and skipped, so it must \
         append no `decode_failed` dead letter; stream held {:?}",
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
    let upstream_store: Arc<dyn EventStore> = Arc::new(DuplicatingStore { inner });
    let source = AggregateId::new("duplicate-order");
    append_order_placed(&upstream_store, &source, 1, "SKU-1").await;

    let saga_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let handled: Handled = Arc::new(Mutex::new(Vec::new()));
    let saga_id = SagaId::new(SAGA_ID);

    let _proxy = spawn_deciding(
        &ps,
        &saga_store,
        from_source(&upstream_store, &source),
        &handled,
    )
    .await;

    wait_for_saga_rows(&saga_store, &saga_id, CONSUMED_POSITION_MARKER, 1).await;
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

/// Given a saga whose replay recovered an unrecoverable tell failure,
/// When a consumed input is redelivered before any new input arrives,
/// Then the failure is still announced to the next input that does reach
/// `handle`: a record that never reaches the saga must not consume the one
/// report it was owed.
#[tokio::test]
async fn a_skipped_input_does_not_consume_the_failed_tell_report() {
    let ps = ProcessSystem::new().await;

    let upstream_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let source = AggregateId::new("drain-order");
    append_order_placed(&upstream_store, &source, 1, "SKU-1").await;
    append_order_placed(&upstream_store, &source, 2, "SKU-2").await;

    let saga_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let saga_id = SagaId::new(SAGA_ID);
    seed_saga_row(
        &saga_store,
        &saga_id,
        1,
        OUTBOX_MARKER,
        encode_outbox_tell_requested_with_target(1, None, "drain-target"),
    )
    .await;
    seed_saga_row(
        &saga_store,
        &saga_id,
        2,
        OUTBOX_MARKER,
        encode_outbox_tell_failed(1),
    )
    .await;
    seed_consumed_position(&saga_store, &saga_id, 3, &source, 1).await;

    let handled: Handled = Arc::new(Mutex::new(Vec::new()));
    // A `Global` cursor keeps its configured position, so seq 1 really is
    // redelivered and really does meet the skip.
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
            source: source.as_str().to_owned(),
            sequence: 2,
            failed_tell_ids: vec![1],
        }],
        "the skipped seq 1 must not have drained the failure: the report is \
         delivered once, and seq 2 is the first invocation entitled to it; \
         stream held {:?}",
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

    let upstream_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let source = AggregateId::new("fold-order");
    append_order_placed(&upstream_store, &source, 1, "SKU-1").await;
    append_order_placed(&upstream_store, &source, 2, "SKU-2").await;

    let saga_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
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

    let upstream_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let source = AggregateId::new("fold-max-order");
    for (sequence, sku) in [(1u64, "SKU-1"), (2, "SKU-2"), (3, "SKU-3")] {
        append_order_placed(&upstream_store, &source, sequence, sku).await;
    }

    let saga_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
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

// CP-FOLD-POISON — an unreadable position is reported and skipped

/// Given a position row whose payload carries no `oneof` at all,
/// When the journal is folded,
/// Then it is skipped rather than read as a position of its own: replay
/// carries on, and the input it would falsely have covered still reaches
/// `handle`.
#[tokio::test]
async fn an_unreadable_position_row_is_skipped_rather_than_read_as_a_default() {
    let ps = ProcessSystem::new().await;

    let upstream_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let source = AggregateId::new("poison-order");
    append_order_placed(&upstream_store, &source, 1, "SKU-1").await;

    let saga_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let saga_id = SagaId::new(SAGA_ID);
    seed_saga_row(
        &saga_store,
        &saga_id,
        1,
        CONSUMED_POSITION_MARKER,
        Bytes::new(),
    )
    .await;

    let handled: Handled = Arc::new(Mutex::new(Vec::new()));
    let _proxy = spawn_deciding(
        &ps,
        &saga_store,
        from_source(&upstream_store, &source),
        &handled,
    )
    .await;

    let barrier = wait_for_handled(&handled, 1).await;
    let events = wait_for_saga_rows(&saga_store, &saga_id, CONSUMED_POSITION_MARKER, 2).await;

    assert_eq!(
        barrier.iter().map(|s| s.sequence).collect::<Vec<_>>(),
        vec![1],
        "an unreadable row says nothing about what was consumed, so the saga \
         must still start and handle its input; stream held {:?}",
        describe(&events)
    );
    assert_eq!(
        position_rows(&events),
        2,
        "the poisoned row is left where it is — skipping it is a read-side \
         decision, not a rewrite; stream held {:?}",
        describe(&events)
    );
    assert_eq!(
        positions(&events),
        vec![ConsumedPayload {
            source: source.as_str().to_owned(),
            sequence: 1,
        }],
        "the only readable position is the one this incarnation wrote; a row \
         read as a default would claim source \"\" at sequence 0; stream held \
         {:?}",
        describe(&events)
    );
}

/// Given an unreadable position row followed by a readable one,
/// When the journal is folded,
/// Then the readable one still takes effect: one poisoned row must not abort
/// the replay that the rows after it depend on.
#[tokio::test]
async fn a_poisoned_position_row_does_not_abort_the_rest_of_the_replay() {
    let ps = ProcessSystem::new().await;

    let upstream_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let source = AggregateId::new("poison-then-valid-order");
    append_order_placed(&upstream_store, &source, 1, "SKU-1").await;
    append_order_placed(&upstream_store, &source, 2, "SKU-2").await;

    let saga_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let saga_id = SagaId::new(SAGA_ID);
    seed_saga_row(
        &saga_store,
        &saga_id,
        1,
        CONSUMED_POSITION_MARKER,
        Bytes::new(),
    )
    .await;
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
        vec![2],
        "the readable position after the poisoned row must still be folded; \
         stream held {:?}",
        describe(&events)
    );
}

// CP-PER-SOURCE — positions belong to the source they were read from

/// Given a position recorded for upstream stream A,
/// When a record of upstream stream B at the same sequence arrives,
/// Then it reaches `handle`: a position covers the source it was read from and
/// nothing else, which is what a subscription over several sources will need.
#[tokio::test]
async fn a_position_of_one_source_does_not_cover_another() {
    let ps = ProcessSystem::new().await;

    let upstream_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let source_a = AggregateId::new("per-source-a");
    let source_b = AggregateId::new("per-source-b");
    append_order_placed(&upstream_store, &source_a, 1, "SKU-A1").await;
    append_order_placed(&upstream_store, &source_b, 1, "SKU-B1").await;

    let saga_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
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
            failed_tell_ids: Vec::new(),
        }],
        "A/1 is consumed and B/1 is not; a single position shared by every \
         source would silently swallow B/1; stream held {:?}",
        describe(&events)
    );
}

// CP-RESUME-STREAM / CP-RESUME-BOUND — where the subscription resumes from

/// Acceptance scenario 4.
///
/// Given positions up to upstream seq 3 on the saga's own stream,
/// When the saga restarts with a subscription configured from the beginning,
/// Then every poll asks upstream for records after seq 3: restart cost stops
/// being a function of how long the upstream history is.
#[tokio::test]
async fn a_stream_subscription_resumes_past_the_folded_position() {
    let ps = ProcessSystem::new().await;

    let queries: Queries = Arc::new(Mutex::new(Vec::new()));
    let upstream_store: Arc<dyn EventStore> = Arc::new(QueryRecordingStore {
        inner: InMemoryEventStore::default(),
        queries: Arc::clone(&queries),
    });
    let source = AggregateId::new("resume-order");
    for (sequence, sku) in [(1u64, "SKU-1"), (2, "SKU-2"), (3, "SKU-3")] {
        append_order_placed(&upstream_store, &source, sequence, sku).await;
    }

    let saga_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let handled: Handled = Arc::new(Mutex::new(Vec::new()));
    let saga_id = SagaId::new(SAGA_ID);

    let first = spawn_deciding(
        &ps,
        &saga_store,
        from_source(&upstream_store, &source),
        &handled,
    )
    .await;
    wait_for_saga_rows(&saga_store, &saga_id, CONSUMED_POSITION_MARKER, 3).await;
    first.stop().await.expect("stop must succeed");
    drop(first);

    take_queries(&queries);

    let _second = spawn_deciding(
        &ps,
        &saga_store,
        from_source(&upstream_store, &source),
        &handled,
    )
    .await;
    append_order_placed(&upstream_store, &source, 4, "SKU-4").await;

    let barrier = wait_for_handled(&handled, 4).await;
    let asked_from: Vec<Option<u64>> = take_queries(&queries)
        .iter()
        .filter(|q| q.stream_key.as_deref() == Some(source.as_str()))
        .map(|q| q.from_stream_sequence)
        .collect();

    assert!(
        !asked_from.is_empty(),
        "the second incarnation must have polled upstream at all"
    );
    assert!(
        asked_from.iter().all(|from| *from >= Some(4)),
        "the subscription resumes past the consumed position — it must never \
         ask for a record it has already decided from; asked from {asked_from:?}"
    );
    assert_eq!(
        barrier.iter().map(|s| s.sequence).collect::<Vec<_>>(),
        vec![1, 2, 3, 4],
        "each input is handled exactly once across both incarnations"
    );
}

/// Given a subscription configured to start past a record the saga has *not*
/// consumed,
/// When the saga restarts with a lower consumed position,
/// Then the configured floor still holds: `with_after` says "do not deliver
/// these", and a consumed position is a second reason to skip, never a reason
/// to start delivering.
#[tokio::test]
async fn the_configured_floor_survives_a_lower_consumed_position() {
    let ps = ProcessSystem::new().await;

    let upstream_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let source = AggregateId::new("floor-order");
    append_order_placed(&upstream_store, &source, 1, "SKU-1").await;

    let saga_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let handled: Handled = Arc::new(Mutex::new(Vec::new()));
    let saga_id = SagaId::new(SAGA_ID);

    let first = spawn_deciding(
        &ps,
        &saga_store,
        from_source(&upstream_store, &source),
        &handled,
    )
    .await;
    wait_for_saga_rows(&saga_store, &saga_id, CONSUMED_POSITION_MARKER, 1).await;
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
        "resuming from the consumed position must not undercut the configured \
         floor: seq 2 and 3 are below it and must never be delivered; stream \
         held {:?}",
        describe(&events)
    );
}

/// Given a `Global` subscription, whose position is a global ordering that a
/// per-source stream sequence cannot be translated into,
/// When the saga restarts,
/// Then the configured cursor is left exactly where it was — de-duplication is
/// the position's job here, and guessing a global position from a per-source
/// one would skip records nobody consumed.
#[tokio::test]
async fn a_global_subscription_keeps_its_configured_cursor() {
    let ps = ProcessSystem::new().await;

    let queries: Queries = Arc::new(Mutex::new(Vec::new()));
    let upstream_store: Arc<dyn EventStore> = Arc::new(QueryRecordingStore {
        inner: InMemoryEventStore::default(),
        queries: Arc::clone(&queries),
    });
    let source = AggregateId::new("global-resume-order");
    append_order_placed(&upstream_store, &source, 1, "SKU-1").await;

    let saga_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let handled: Handled = Arc::new(Mutex::new(Vec::new()));
    let saga_id = SagaId::new(SAGA_ID);

    let first = spawn_deciding(
        &ps,
        &saga_store,
        from_every_source(&upstream_store),
        &handled,
    )
    .await;
    wait_for_saga_rows(&saga_store, &saga_id, CONSUMED_POSITION_MARKER, 1).await;
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

    let upstream_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let source = AggregateId::new("two-batch-order");
    append_order_placed(&upstream_store, &source, 1, "SKU-1").await;

    let saga_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let handled: Handled = Arc::new(Mutex::new(Vec::new()));
    let saga_id = SagaId::new(SAGA_ID);

    let handled_for_producer = Arc::clone(&handled);
    let _proxy =
        SagaProps::<TwoBatchSaga>::new(saga_id.clone(), Arc::clone(&saga_store), move |_| {
            TwoBatchSaga {
                handled: Arc::clone(&handled_for_producer),
            }
        })
        .subscribed_to::<OrderPlaced>(from_source(&upstream_store, &source))
        .spawn(&ps)
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
        vec![ConsumedPayload {
            source: source.as_str().to_owned(),
            sequence: 1,
        }],
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

    let upstream_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let source = AggregateId::new("persist-then-end-order");
    append_order_placed(&upstream_store, &source, 1, "SKU-1").await;

    let saga_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let handled: Handled = Arc::new(Mutex::new(Vec::new()));
    let saga_id = SagaId::new(SAGA_ID);

    let handled_for_producer = Arc::clone(&handled);
    let _proxy =
        SagaProps::<PersistThenEndSaga>::new(saga_id.clone(), Arc::clone(&saga_store), move |_| {
            PersistThenEndSaga {
                handled: Arc::clone(&handled_for_producer),
            }
        })
        .subscribed_to::<OrderPlaced>(from_source(&upstream_store, &source))
        .spawn(&ps)
        .await;

    // The consumed position is the barrier: it only ever lands on the `Ended`
    // append, so waiting for it also waits for `End` to have been
    // interpreted.
    let events = wait_for_saga_rows(&saga_store, &saga_id, CONSUMED_POSITION_MARKER, 1).await;

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
        vec![ConsumedPayload {
            source: source.as_str().to_owned(),
            sequence: 1,
        }],
        "the position must be durable even though it rides the `Ended` append \
         rather than the vector's physically last element; stream held {:?}",
        describe(&events)
    );
}

/// Given one `handle` whose effect reaches the store as two appends, the
/// opening one lands but the closing one — the one the position would ride —
/// never does before the saga stops,
/// When the saga restarts and upstream redelivers the same input,
/// Then `handle` runs again and this time both appends land: the decision the
/// first attempt could not durably record is not lost, and the position is
/// finally committed once, by the append that closes the input out.
#[tokio::test]
async fn a_closing_batch_that_never_landed_is_recovered_by_redelivery() {
    let ps = ProcessSystem::new().await;

    let upstream_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let source = AggregateId::new("two-batch-crash-order");
    append_order_placed(&upstream_store, &source, 1, "SKU-1").await;

    let failing = Arc::new(AtomicBool::new(true));
    let decision_appends = Arc::new(AtomicU64::new(0));
    let saga_store: Arc<dyn EventStore> = Arc::new(FailSecondDecisionAppendStore {
        inner: InMemoryEventStore::default(),
        failing: Arc::clone(&failing),
        decision_appends: Arc::clone(&decision_appends),
    });
    let handled: Handled = Arc::new(Mutex::new(Vec::new()));
    let saga_id = SagaId::new(SAGA_ID);

    let handled_for_first = Arc::clone(&handled);
    let first =
        SagaProps::<TwoBatchSaga>::new(saga_id.clone(), Arc::clone(&saga_store), move |_| {
            TwoBatchSaga {
                handled: Arc::clone(&handled_for_first),
            }
        })
        .subscribed_to::<OrderPlaced>(from_source(&upstream_store, &source))
        .spawn(&ps)
        .await;

    // The opening batch lands (one `ReservationRequested` row), the closing
    // batch keeps failing, and the exhausted retries end in a dead letter —
    // the barrier that says interpretation of this record is over.
    let events = wait_for_saga_rows(&saga_store, &saga_id, DEAD_LETTER_MARKER, 1).await;
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

    first.stop().await.expect("stop must succeed");
    drop(first);

    // The store recovers; a fresh incarnation replays the same upstream
    // record.
    failing.store(false, Ordering::SeqCst);

    let handled_for_second = Arc::clone(&handled);
    let _second =
        SagaProps::<TwoBatchSaga>::new(saga_id.clone(), Arc::clone(&saga_store), move |_| {
            TwoBatchSaga {
                handled: Arc::clone(&handled_for_second),
            }
        })
        .subscribed_to::<OrderPlaced>(from_source(&upstream_store, &source))
        .spawn(&ps)
        .await;

    let events = wait_for_saga_rows(&saga_store, &saga_id, CONSUMED_POSITION_MARKER, 1).await;

    let invocations = seen(&handled);
    assert_eq!(
        invocations
            .iter()
            .filter(|s| s.source == source.as_str() && s.sequence == 1)
            .count(),
        2,
        "no position was durable after the first attempt, so redelivery must \
         reach handle again; invocations: {invocations:?}"
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
        vec![ConsumedPayload {
            source: source.as_str().to_owned(),
            sequence: 1,
        }],
        "the position is committed exactly once, by the batch that finally \
         closes the input out; stream held {:?}",
        describe(&events)
    );
}

/// Given one `handle` whose effect reaches the store as two appends, the
/// opening one exhausting its retries — its failure is reported as a dead
/// letter, which is why interpretation carries on to the closing append at all,
/// When the saga restarts and upstream redelivers the same input,
/// Then nothing on the stream says the input was consumed and `handle` runs
/// again: an input is consumed only once *every* append it required has landed,
/// so the decision the opening batch lost is re-made rather than skipped
/// forever by a position the closing batch had already committed.
#[tokio::test]
async fn an_input_whose_opening_batch_never_landed_is_not_recorded_as_consumed() {
    let ps = ProcessSystem::new().await;

    let upstream_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let source = AggregateId::new("two-batch-opening-crash-order");
    append_order_placed(&upstream_store, &source, 1, "SKU-1").await;

    let saga_store: Arc<dyn EventStore> = Arc::new(FailUntilReportedStore {
        inner: InMemoryEventStore::default(),
        reported: AtomicBool::new(false),
    });
    let handled: Handled = Arc::new(Mutex::new(Vec::new()));
    let saga_id = SagaId::new(SAGA_ID);

    let handled_for_first = Arc::clone(&handled);
    let first =
        SagaProps::<TwoBatchSaga>::new(saga_id.clone(), Arc::clone(&saga_store), move |_| {
            TwoBatchSaga {
                handled: Arc::clone(&handled_for_first),
            }
        })
        .subscribed_to::<OrderPlaced>(from_source(&upstream_store, &source))
        .spawn(&ps)
        .await;

    // Two barriers, in the order the interpretation reaches them: the dead
    // letter says the opening batch is done failing, and the one decision row
    // that follows it says the closing batch — the only append for this input
    // a position could have ridden — has landed.  After that this input's
    // interpretation is over, so what the stream does not hold, it never will.
    wait_for_saga_rows(&saga_store, &saga_id, DEAD_LETTER_MARKER, 1).await;
    let events =
        wait_for_saga_rows(&saga_store, &saga_id, ReservationRequested::EVENT_TYPE, 1).await;

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

    first.stop().await.expect("stop must succeed");
    drop(first);

    let handled_for_second = Arc::clone(&handled);
    let _second =
        SagaProps::<TwoBatchSaga>::new(saga_id.clone(), Arc::clone(&saga_store), move |_| {
            TwoBatchSaga {
                handled: Arc::clone(&handled_for_second),
            }
        })
        .subscribed_to::<OrderPlaced>(from_source(&upstream_store, &source))
        .spawn(&ps)
        .await;

    let events = wait_for_saga_rows(&saga_store, &saga_id, CONSUMED_POSITION_MARKER, 1).await;

    let invocations = seen(&handled);
    assert_eq!(
        invocations
            .iter()
            .filter(|s| s.source == source.as_str() && s.sequence == 1)
            .count(),
        2,
        "no position was durable after the first attempt, so redelivery must \
         reach handle again; invocations: {invocations:?}"
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
        vec![ConsumedPayload {
            source: source.as_str().to_owned(),
            sequence: 1,
        }],
        "the position is committed exactly once, by the attempt that landed \
         every append the input required; stream held {:?}",
        describe(&events)
    );
}

/// Given one `handle` whose effect reaches the store as two appends — an
/// opening `Persist` and a closing `End` — the opening one exhausting its
/// retries before the closing append is ever attempted,
/// When the closing `End` would otherwise run anyway,
/// Then no durable `Ended` marker is written for it: closing the input out
/// here would let the revival guard refuse every future spawn, permanently
/// losing the opening decision that never landed. Only after the saga
/// restarts and upstream redelivers the same input does `handle` run again,
/// this time landing both appends and finally terminating the saga with the
/// position riding the same append as `Ended`.
#[tokio::test]
async fn an_input_whose_opening_batch_never_landed_leaves_a_closing_end_unwritten() {
    let ps = ProcessSystem::new().await;

    let upstream_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let source = AggregateId::new("opening-crash-then-end-order");
    append_order_placed(&upstream_store, &source, 1, "SKU-1").await;

    let saga_store: Arc<dyn EventStore> = Arc::new(FailUntilReportedStore {
        inner: InMemoryEventStore::default(),
        reported: AtomicBool::new(false),
    });
    let handled: Handled = Arc::new(Mutex::new(Vec::new()));
    let saga_id = SagaId::new(SAGA_ID);

    let handled_for_first = Arc::clone(&handled);
    let first =
        SagaProps::<PersistThenEndSaga>::new(saga_id.clone(), Arc::clone(&saga_store), move |_| {
            PersistThenEndSaga {
                handled: Arc::clone(&handled_for_first),
            }
        })
        .subscribed_to::<OrderPlaced>(from_source(&upstream_store, &source))
        .spawn(&ps)
        .await;

    // The dead letter says the opening batch is done failing. The closing
    // `End` does not carry `ReservationRequested`, so `FailUntilReportedStore`
    // would have accepted it had the interpreter attempted it — this barrier
    // gives that append a chance to happen before the assertions below check
    // that it did not.
    let events = wait_for_saga_rows(&saga_store, &saga_id, DEAD_LETTER_MARKER, 1).await;

    assert!(
        decisions(&events).is_empty(),
        "the opening batch never landed, so no decision may be on the stream; \
         stream held {:?}",
        describe(&events)
    );
    assert!(
        !events
            .iter()
            .any(|e| matches!(outbox_kind_of(e), Some(OutboxKind::Ended(_)))),
        "a preceding append that never landed must withhold the closing `End` \
         marker too — durably ending the saga here would refuse every future \
         revival and lose the opening decision for good; stream held {:?}",
        describe(&events)
    );
    assert!(
        positions(&events).is_empty(),
        "an append this input required never landed, so the input is not \
         consumed and no position may be on the stream; stream held {:?}",
        describe(&events)
    );

    first.stop().await.expect("stop must succeed");
    drop(first);

    let handled_for_second = Arc::clone(&handled);
    let _second =
        SagaProps::<PersistThenEndSaga>::new(saga_id.clone(), Arc::clone(&saga_store), move |_| {
            PersistThenEndSaga {
                handled: Arc::clone(&handled_for_second),
            }
        })
        .subscribed_to::<OrderPlaced>(from_source(&upstream_store, &source))
        .spawn(&ps)
        .await;

    let events = wait_for_saga_rows(&saga_store, &saga_id, CONSUMED_POSITION_MARKER, 1).await;

    let invocations = seen(&handled);
    assert_eq!(
        invocations
            .iter()
            .filter(|s| s.source == source.as_str() && s.sequence == 1)
            .count(),
        2,
        "no position was durable after the first attempt, so redelivery must \
         reach handle again; invocations: {invocations:?}"
    );
    assert_eq!(
        decisions(&events).len(),
        1,
        "the opening decision the first attempt lost is re-made by the \
         second, successful attempt — the trailing Persist after End is still \
         never interpreted; stream held {:?}",
        describe(&events)
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(outbox_kind_of(e), Some(OutboxKind::Ended(_)))),
        "the second attempt lands every append this input requires, so `End` \
         finally closes it out; stream held {:?}",
        describe(&events)
    );
    assert_eq!(
        positions(&events),
        vec![ConsumedPayload {
            source: source.as_str().to_owned(),
            sequence: 1,
        }],
        "the position is committed exactly once, by the `Ended` append that \
         finally closes the input out; stream held {:?}",
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
/// a schedule-marker append is not retried and enqueues no dead letter, so if
/// the interpreter treated its failure as an ordinary "carry on" the closing
/// append would record the input as consumed and the cancellation would be lost
/// for good.
#[tokio::test]
async fn an_input_whose_middle_append_never_landed_is_not_recorded_as_consumed() {
    let ps = ProcessSystem::new().await;

    let upstream_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let source = AggregateId::new("middle-append-crash-order");
    append_order_placed(&upstream_store, &source, 1, "SKU-1").await;

    let failing = Arc::new(AtomicBool::new(true));
    let saga_store: Arc<dyn EventStore> = Arc::new(FailScheduleMarkerAppendStore {
        inner: InMemoryEventStore::default(),
        failing: Arc::clone(&failing),
    });
    let handled: Handled = Arc::new(Mutex::new(Vec::new()));
    let saga_id = SagaId::new(SAGA_ID);

    let handled_for_first = Arc::clone(&handled);
    let first =
        SagaProps::<TwoBatchSaga>::new(saga_id.clone(), Arc::clone(&saga_store), move |_| {
            TwoBatchSaga {
                handled: Arc::clone(&handled_for_first),
            }
        })
        .subscribed_to::<OrderPlaced>(from_source(&upstream_store, &source))
        .spawn(&ps)
        .await;

    // Both `Persist` batches landing is the barrier: the closing one is the
    // last append this input makes, so once its decision is on the stream what
    // the stream does not hold, it never will.
    let events =
        wait_for_saga_rows(&saga_store, &saga_id, ReservationRequested::EVENT_TYPE, 2).await;

    assert!(
        !events.iter().any(|e| e.event_type == SCHEDULE_MARKER),
        "the middle append must have been the one that failed; stream held {:?}",
        describe(&events)
    );
    assert!(
        positions(&events).is_empty(),
        "an append this input required never landed, so the input is not \
         consumed and no position may be on the stream; stream held {:?}",
        describe(&events)
    );

    first.stop().await.expect("stop must succeed");
    drop(first);

    // The store recovers; a fresh incarnation replays the same upstream record.
    failing.store(false, Ordering::SeqCst);

    let handled_for_second = Arc::clone(&handled);
    let _second =
        SagaProps::<TwoBatchSaga>::new(saga_id.clone(), Arc::clone(&saga_store), move |_| {
            TwoBatchSaga {
                handled: Arc::clone(&handled_for_second),
            }
        })
        .subscribed_to::<OrderPlaced>(from_source(&upstream_store, &source))
        .spawn(&ps)
        .await;

    let events = wait_for_saga_rows(&saga_store, &saga_id, CONSUMED_POSITION_MARKER, 1).await;

    let invocations = seen(&handled);
    assert_eq!(
        invocations
            .iter()
            .filter(|s| s.source == source.as_str() && s.sequence == 1)
            .count(),
        2,
        "no position was durable after the first attempt, so redelivery must \
         reach handle again; invocations: {invocations:?}"
    );
    assert_eq!(
        positions(&events),
        vec![ConsumedPayload {
            source: source.as_str().to_owned(),
            sequence: 1,
        }],
        "the position is committed exactly once, by the attempt that landed \
         every append the input required; stream held {:?}",
        describe(&events)
    );
}

// CP-EMPTY-STILL-CONSUMED — a decision-less input is still consumed

/// Given a `handle` that returns an all-empty `Persist` — no user event, tell
/// or schedule for the interpreter to batch,
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

    let upstream_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let source = AggregateId::new("empty-persist-order");
    append_order_placed(&upstream_store, &source, 1, "SKU-1").await;

    let saga_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let handled: Handled = Arc::new(Mutex::new(Vec::new()));
    let saga_id = SagaId::new(SAGA_ID);

    let handled_for_first = Arc::clone(&handled);
    let first =
        SagaProps::<EmptyPersistSaga>::new(saga_id.clone(), Arc::clone(&saga_store), move |_| {
            EmptyPersistSaga {
                handled: Arc::clone(&handled_for_first),
            }
        })
        .subscribed_to::<OrderPlaced>(from_source(&upstream_store, &source))
        .spawn(&ps)
        .await;

    wait_for_saga_rows(&saga_store, &saga_id, CONSUMED_POSITION_MARKER, 1).await;
    first.stop().await.expect("stop must succeed");
    drop(first);

    let handled_for_second = Arc::clone(&handled);
    let _second =
        SagaProps::<EmptyPersistSaga>::new(saga_id.clone(), Arc::clone(&saga_store), move |_| {
            EmptyPersistSaga {
                handled: Arc::clone(&handled_for_second),
            }
        })
        .subscribed_to::<OrderPlaced>(from_source(&upstream_store, &source))
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
        positions(&events)
            .iter()
            .map(|p| p.sequence)
            .collect::<Vec<_>>(),
        vec![1, 2],
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

    let upstream_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let source = AggregateId::new("timer-order");
    append_order_placed(&upstream_store, &source, 1, "SKU-1").await;

    let batches: Batches = Arc::new(Mutex::new(Vec::new()));
    let saga_store: Arc<dyn EventStore> = Arc::new(RecordingStore {
        inner: InMemoryEventStore::default(),
        batches: Arc::clone(&batches),
    });
    let fired = Arc::new(AtomicU64::new(0));
    let saga_id = SagaId::new(SAGA_ID);

    let fired_for_producer = Arc::clone(&fired);
    let _proxy = SagaProps::<SchedulingSaga>::new(
        saga_id.clone(),
        Arc::clone(&saga_store),
        move |components| SchedulingSaga {
            deadline: components.deadline(),
            fired: Arc::clone(&fired_for_producer),
        },
    )
    // No scheduler wiring: the alphabet declaring a deadline is the whole
    // declaration, and the host resolves the resident timer core from it.
    .subscribed_to::<OrderPlaced>(from_source(&upstream_store, &source))
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
        vec![ConsumedPayload {
            source: source.as_str().to_owned(),
            sequence: 1,
        }],
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
                .filter(|e| e.event_type == CONSUMED_POSITION_MARKER)
                .count(),
            0,
            "the append a timer firing produced carries no position row; batch \
             held {rows:?}"
        );
    }
}

// CP-DLQ — an upstream record a dead letter settles commits its own position

/// `Saga::handle` always fails, so every upstream record it is handed becomes
/// a `HandleFailed` dead letter instead of a decision.
struct AlwaysFailingSaga {
    handled: Handled,
}

/// A concrete `Saga::Error` so `handle` has something to fail with.
#[derive(Debug)]
struct HandleBoom;

impl std::fmt::Display for HandleBoom {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "handle always fails")
    }
}

impl std::error::Error for HandleBoom {}

#[async_trait]
impl Saga for AlwaysFailingSaga {
    type Subscribe = Observed;
    type Event = ReservationRequested;
    type Error = HandleBoom;

    fn correlate(_input: &Self::Subscribe) -> Option<SagaId> {
        Some(SagaId::new(SAGA_ID))
    }

    async fn handle(
        &mut self,
        _input: Self::Subscribe,
        ctx: &mut SagaContext,
    ) -> Result<SagaEffect<Self::Event>, Self::Error> {
        record(&self.handled, ctx);
        Err(HandleBoom)
    }
}

async fn spawn_always_failing(
    ps: &ProcessSystem,
    saga_store: &Arc<dyn EventStore>,
    subscription: Subscription,
    handled: &Handled,
) -> nitinol_saga::SagaProxy<AlwaysFailingSaga> {
    let handled = Arc::clone(handled);
    SagaProps::<AlwaysFailingSaga>::new(
        SagaId::new(SAGA_ID),
        Arc::clone(saga_store),
        move |_components| AlwaysFailingSaga {
            handled: Arc::clone(&handled),
        },
    )
    .subscribed_to::<OrderPlaced>(subscription)
    .spawn(ps)
    .await
}

/// Given a saga whose `handle` fails for every upstream record it sees,
/// When upstream seq 1 is delivered and settled as a `HandleFailed` dead
/// letter — an `Ok` outcome the `DirectPoller` advances its cursor past —
/// Then seq 1's consumed position lands in the same append as the dead
/// letter, the same atomic-batch guarantee a successful decision gets, and a
/// restart does not redeliver seq 1 into `handle` a second time.
///
/// Before the fix this dead letter settled the record without ever writing
/// its position: the DLQ append carried the `handle_failed` row alone, so a
/// restart's replay found nothing to skip and redelivered seq 1 into `handle`
/// again, producing a second `handle_failed` dead letter for the same input.
#[tokio::test]
async fn a_handle_failed_dead_letter_commits_its_consumed_position() {
    let ps = ProcessSystem::new().await;

    let upstream_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let source = AggregateId::new("handle-failed-order");
    append_order_placed(&upstream_store, &source, 1, "SKU-1").await;

    let saga_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let handled: Handled = Arc::new(Mutex::new(Vec::new()));
    let saga_id = SagaId::new(SAGA_ID);

    let first = spawn_always_failing(
        &ps,
        &saga_store,
        from_source(&upstream_store, &source),
        &handled,
    )
    .await;

    // The dead letter and the position it settles land in one append, so by
    // the time either row is visible the other already is too.
    let events = wait_for_saga_rows(&saga_store, &saga_id, CONSUMED_POSITION_MARKER, 1).await;

    assert_eq!(
        wait_for_handled(&handled, 1).await.len(),
        1,
        "handle must have run once to produce the failure this test settles; \
         stream held {:?}",
        describe(&events)
    );
    assert_eq!(
        count_dead_letter_kind(&events, "handle_failed"),
        1,
        "the first incarnation's one failed attempt must be the only \
         handle_failed dead letter so far; stream held {:?}",
        describe(&events)
    );
    assert_eq!(
        positions(&events),
        vec![ConsumedPayload {
            source: source.as_str().to_owned(),
            sequence: 1,
        }],
        "a HandleFailed dead letter must commit its own input's consumed \
         position — the same atomic-batch guarantee a successful decision \
         gets; stream held {:?}",
        describe(&events)
    );

    first.stop().await.expect("stop must succeed");
    drop(first);

    let _second = spawn_always_failing(
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
        "seq 1 was already settled by the first incarnation's dead letter; \
         the second incarnation must see only seq 2, not seq 1 redelivered \
         into handle a second time; stream held {:?}",
        describe(&events)
    );
    assert_eq!(
        count_dead_letter_kind(&events, "handle_failed"),
        2,
        "one handle_failed dead letter per input actually handled — seq 1 \
         once from the first incarnation, seq 2 once from the second; a \
         redelivered seq 1 would have produced a third; stream held {:?}",
        describe(&events)
    );
}

/// Given a saga whose `handle` fails for the upstream record it is handed,
/// When the framework settles that record as a `HandleFailed` dead letter,
/// Then a single append carries both the dead letter and the position it
/// settles: neither is ever in the store without the other.
///
/// Its sibling above fixes that both rows end up durable and that a restart
/// does not redeliver the record.  That same outcome is reached by writing the
/// dead letter first and the position in a second append — so the sibling
/// passes under that implementation too.  It is not equivalent: a store
/// failure between the two appends leaves the record settled by a durable dead
/// letter carrying no position, and the caller has already been told the dead
/// letter landed, so the cursor moves past a record the fold cannot restore as
/// consumed.  That is the redelivery the position exists to stop, and only the
/// batch boundary tells the two implementations apart.
#[tokio::test]
async fn a_dead_letter_and_the_position_it_settles_reach_the_store_in_one_append() {
    let ps = ProcessSystem::new().await;

    let upstream_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let source = AggregateId::new("atomic-dead-letter-order");
    append_order_placed(&upstream_store, &source, 1, "SKU-1").await;

    let batches: Batches = Arc::new(Mutex::new(Vec::new()));
    let saga_store: Arc<dyn EventStore> = Arc::new(RecordingStore {
        inner: InMemoryEventStore::default(),
        batches: Arc::clone(&batches),
    });
    let handled: Handled = Arc::new(Mutex::new(Vec::new()));

    let _proxy = spawn_always_failing(
        &ps,
        &saga_store,
        from_source(&upstream_store, &source),
        &handled,
    )
    .await;

    let saga_id = SagaId::new(SAGA_ID);
    let events = wait_for_saga_rows(&saga_store, &saga_id, CONSUMED_POSITION_MARKER, 1).await;

    let recorded = batches
        .lock()
        .expect("recorded batches must not be poisoned")
        .clone();

    let dead_letter_batches: Vec<&Vec<RecordedEvent>> = recorded
        .iter()
        .filter(|batch| batch.iter().any(|e| e.event_type == DEAD_LETTER_MARKER))
        .collect();
    assert_eq!(
        dead_letter_batches.len(),
        1,
        "the one failed input must have produced exactly one dead-letter \
         append for this test to say anything about it; stream held {:?}",
        describe(&events)
    );

    let batch = dead_letter_batches[0];
    let rows: Vec<String> = batch
        .iter()
        .map(|e| format!("{}#{}", e.sequence, e.event_type))
        .collect();
    assert_eq!(
        batch
            .iter()
            .filter(|e| e.event_type == CONSUMED_POSITION_MARKER)
            .count(),
        1,
        "the position of the record this dead letter settles must ride the \
         dead letter's own append, the same atomic batch a decision's position \
         rides; batch held {rows:?}"
    );

    let position_rows_appended = recorded
        .iter()
        .flatten()
        .filter(|e| e.event_type == CONSUMED_POSITION_MARKER)
        .count();
    assert_eq!(
        position_rows_appended,
        1,
        "and it is committed only there — a position written by an append of \
         its own would leave a window where the settled record has none; \
         stream held {:?}",
        describe(&events)
    );

    let position = batch
        .iter()
        .find(|e| e.event_type == CONSUMED_POSITION_MARKER)
        .expect("the dead letter's batch carries the position asserted above");
    assert_eq!(
        common::decode_consumed_position(&position.payload),
        ConsumedPayload {
            source: source.as_str().to_owned(),
            sequence: 1,
        },
        "the position names the upstream stream and the sequence this dead \
         letter settled; batch held {rows:?}"
    );

    let stamp = batch[0].occurred_at;
    assert!(
        batch.iter().all(|e| e.occurred_at == stamp),
        "every row of one commit carries the commit's own timestamp; a position \
         stamped from a second clock reading would break the one signal a reader \
         has that the rows landed together; batch held {rows:?}"
    );
}
