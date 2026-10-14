//! The single-atomic-batch boundary of a `Persist`, read off the store.
//!
//! A domain event and the schedule fact of a deadline set in the same decision
//! occupy consecutive stream sequences either way, so sequence numbers alone
//! cannot tell "one atomic append of two rows" apart from "two appends of one
//! row each".  The batch boundary of each `append` call is therefore recorded
//! directly by a decorating [`RecordingStore`]; that grouping is the only
//! observation that distinguishes the merged behaviour from the window a split
//! append opens — a crash between the two appends would arm a timer against a
//! decision no store holds, or leave a decision whose deadline no stream
//! records.
//!
//! Two claims live here, and they pull in opposite directions:
//!
//! - Every adjacent `Persist × Persist` junction folds, however the effect was
//!   built: through `combine`, or hand-assembled as a `Sequence` — every
//!   `SagaEffect` variant is public — including one with an empty nested
//!   `Sequence` or the identity `None` wedged between the two leaves.  The
//!   `nitinol.saga.consumed` position of the input the decision was made from
//!   closes that same append.
//! - `CancelSchedule` and `End` are hard boundaries the fold never crosses, so
//!   there one `handle` call reaching the store as *two* appends is the
//!   contract rather than a regression.  The position rides whichever append
//!   lands last, never an earlier one: recorded sooner, it would make "this
//!   input is dealt with" durable while a decision the same input produced is
//!   not.
//!
//! Run with: `cargo test -p nitinol-saga --test saga_combine_persist_single_append`.

#[path = "common/helpers.rs"]
mod common;
use common::{
    decode_consumed_fact, decode_lifecycle_kind, decode_schedule_kind, JsonCodec, LifecycleKind,
    ScheduleKind, CONSUMED_MARKER, LIFECYCLE_MARKER, SCHEDULE_MARKER,
};

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use serde::{Deserialize, Serialize};
use tokio::sync::Notify;

use nitinol_eventsource::codec::Codec;
use nitinol_eventsource::Event;
use nitinol_persistence::error::{AppendError, LoadError};
use nitinol_persistence::store::{EventStore, EventStream, InMemoryEventStore, Journal};
use nitinol_persistence::{
    AggregateId, AppendOutcome, AppendingEvent, EventType, Family, LoadQuery, TypeName,
};
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
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("combine_atomic"), TypeName::new("OrderPlaced"));
    type Codec = JsonCodec;
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ReservationRequested {
    sku: String,
}

impl Event for ReservationRequested {
    const EVENT_TYPE: EventType = EventType::new(
        Family::new("combine_atomic"),
        TypeName::new("ReservationRequested"),
    );
    type Codec = JsonCodec;
}

/// The saga's promise to its future self, carried inside the schedule fact.
/// It is persisted but is not an event, so it names the `Codec` that writes it
/// down rather than relying on serde.
#[derive(Clone, Debug, PartialEq)]
struct Reminder {
    note: String,
}

/// The bytes a `Reminder` is written as are the note itself — deliberately not
/// a JSON shape, so a payload produced by anything but this codec would be
/// visibly different on the stream.
struct ReminderCodec;

impl Codec<Reminder> for ReminderCodec {
    type Error = std::convert::Infallible;

    fn encode(value: &Reminder) -> Result<Bytes, Self::Error> {
        Ok(Bytes::from(value.note.clone().into_bytes()))
    }

    fn decode(payload: &[u8]) -> Result<Reminder, Self::Error> {
        Ok(Reminder {
            note: String::from_utf8_lossy(payload).into_owned(),
        })
    }
}

impl DeadlineMessage for Reminder {
    const TYPE_NAME: &'static str = "reminder";
    const CODEC_NAME: &'static str = "reminder-codec";
    type Codec = ReminderCodec;
}

/// Every deadline this file sets is an hour out, so the `scheduled` fact is the
/// only row a timer ever contributes here: no firing — and so no `fired`
/// marker, and no second decision — can disturb the batch grouping under test.
const NEVER_DUE: Duration = Duration::from_secs(3600);

// Observing the batch boundary of each append

#[derive(Clone, Debug)]
struct RecordedEvent {
    event_type: EventType,
    payload: Bytes,
}

type Batches = Arc<Mutex<Vec<Vec<RecordedEvent>>>>;

/// `EventStore` decorator that appends one entry to `batches` per successful
/// `append` call, preserving the events of that call as a group.
#[derive(Clone)]
struct RecordingStore {
    inner: InMemoryEventStore,
    batches: Batches,
}

impl RecordingStore {
    fn new(batches: Batches) -> Self {
        Self {
            inner: InMemoryEventStore::default(),
            batches,
        }
    }
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
                event_type: e.event_type,
                payload: e.payload.clone(),
            })
            .collect();
        // Record only after the append durably succeeds: a failed attempt never
        // produced a batch boundary in the stream.
        let outcome = self.inner.append(key, events).await?;
        self.batches
            .lock()
            .expect("recording store batch buffer must not be poisoned")
            .push(recorded);
        Ok(outcome)
    }

    async fn load(&self, query: LoadQuery) -> Result<EventStream<'_>, LoadError> {
        self.inner.load(query).await
    }
}

fn is_domain_event(event: &RecordedEvent) -> bool {
    event.event_type == ReservationRequested::EVENT_TYPE
}

/// The transition a schedule row records, or `None` when the row is not one.
///
/// `nitinol.saga.schedule` is a single identity whose payload `oneof` carries
/// which transition the row states, so telling a `scheduled` fact from a
/// `cancelled` one means decoding the payload — the column alone cannot say.
fn schedule_kind(event: &RecordedEvent) -> Option<ScheduleKind> {
    (event.event_type == SCHEDULE_MARKER).then(|| decode_schedule_kind(&event.payload))
}

fn is_scheduled_fact(event: &RecordedEvent) -> bool {
    matches!(schedule_kind(event), Some(ScheduleKind::Scheduled(_)))
}

fn is_cancelled_fact(event: &RecordedEvent) -> bool {
    matches!(schedule_kind(event), Some(ScheduleKind::Cancelled(_)))
}

/// Timer names of the `scheduled` facts inside `batch`, in the order they were
/// appended — the merged `schedules` order, made observable.
fn scheduled_timer_names(batch: &[RecordedEvent]) -> Vec<String> {
    batch
        .iter()
        .filter_map(|e| match schedule_kind(e) {
            Some(ScheduleKind::Scheduled(payload)) => Some(payload.name),
            _ => None,
        })
        .collect()
}

/// The position of the upstream input the batch was decided from — framework
/// bookkeeping that rides whichever append closes that input out.
fn is_consumed_position(event: &RecordedEvent) -> bool {
    event.event_type == CONSUMED_MARKER
}

/// The durable `Ended` fact `SagaEffect::End` writes before the process stops.
fn is_ended_fact(event: &RecordedEvent) -> bool {
    event.event_type == LIFECYCLE_MARKER
        && matches!(
            decode_lifecycle_kind(&event.payload),
            LifecycleKind::Ended(_)
        )
}

/// Render one row the way an operator reading the stream would: the
/// declaration, plus the fact its payload carries where the column alone does
/// not say.
fn describe_row(event: &RecordedEvent) -> String {
    if let Some(kind) = schedule_kind(event) {
        return match kind {
            ScheduleKind::Scheduled(p) => format!("schedule:scheduled({})", p.name),
            ScheduleKind::Cancelled(p) => format!("schedule:cancelled({})", p.name),
            ScheduleKind::Fired(p) => format!("schedule:fired({})", p.name),
        };
    }
    if event.event_type == CONSUMED_MARKER {
        let fact = decode_consumed_fact(&event.payload);
        return format!("consumed({}@{})", fact.source, fact.sequence);
    }
    if event.event_type == LIFECYCLE_MARKER {
        return match decode_lifecycle_kind(&event.payload) {
            LifecycleKind::Ended(_) => "lifecycle:ended".to_owned(),
        };
    }
    event.event_type.to_string()
}

fn describe(batches: &[Vec<RecordedEvent>]) -> Vec<Vec<String>> {
    batches
        .iter()
        .map(|batch| batch.iter().map(describe_row).collect())
        .collect()
}

// Alphabets

/// The alphabet every saga in this file but [`PersistThenEndSaga`] reacts to:
/// one observed source, and the saga's own time.  Declaring the deadline
/// variant is the whole of the scheduler wiring — the host resolves its
/// resident timer core from it, and `SagaComponents::deadline` only hands out a
/// `Deadline<Reminder>` to a saga that declared one.
#[derive(SagaSubscribe)]
enum ScheduleInput {
    OrderPlaced(OrderPlaced),
    #[subscribe(deadline)]
    Reminder(Reminder),
}

/// The alphabet of [`PersistThenEndSaga`]: one observed source, no deadline.
/// Terminating needs no timer, and a `Deadline` field nothing reads would
/// suggest the boundary under test had something to do with scheduling.
#[derive(SagaSubscribe)]
enum Observed {
    OrderPlaced(OrderPlaced),
}

/// The arm of [`ScheduleInput`] no scenario in this file reaches: every
/// deadline is [`NEVER_DUE`].  Naming the note keeps the firing's payload part
/// of the alphabet rather than a field nothing ever reads, and the arm writes
/// nothing so a firing could not disturb the batch assertions.
fn never_due_arm(reminder: Reminder) -> SagaEffect<ReservationRequested> {
    let _never_due = reminder.note;
    SagaEffect::None
}

// Sagas

/// Correlation rule of [`PersistCombineScheduleSaga`]: the single reservation
/// process every `OrderPlaced` in this test belongs to.  A deadline firing
/// never travels the subscription path, so its arm answers `None`.
const PERSIST_COMBINE_SCHEDULE_SAGA_ID: &str = "combine-schedule-saga";

/// A decision combined with the deadline it sets — the shape `combine`'s
/// junction fold exists for.
struct PersistCombineScheduleSaga {
    deadline: Deadline<Reminder>,
    handled: Arc<Notify>,
}

#[async_trait]
impl Saga for PersistCombineScheduleSaga {
    type Subscribe = ScheduleInput;
    type Event = ReservationRequested;
    type Error = std::convert::Infallible;

    fn correlate(input: &Self::Subscribe) -> Option<SagaId> {
        match input {
            ScheduleInput::OrderPlaced(_) => Some(SagaId::new(PERSIST_COMBINE_SCHEDULE_SAGA_ID)),
            ScheduleInput::Reminder(_) => None,
        }
    }

    async fn handle(
        &mut self,
        input: Self::Subscribe,
        _ctx: &mut SagaContext,
    ) -> Result<SagaEffect<Self::Event>, Self::Error> {
        match input {
            ScheduleInput::OrderPlaced(event) => {
                let effect = SagaEffect::persist(ReservationRequested {
                    sku: event.sku.clone(),
                })
                .combine(self.deadline.set(
                    TimerName::new("reservation-deadline"),
                    NEVER_DUE,
                    Reminder { note: event.sku },
                ));
                self.handled.notify_one();
                Ok(effect)
            }
            ScheduleInput::Reminder(reminder) => Ok(never_due_arm(reminder)),
        }
    }
}

/// Correlation rule of [`PersistSequencedWithScheduleSaga`]: the single
/// reservation process every `OrderPlaced` in this test belongs to.
const PERSIST_SEQUENCED_WITH_SCHEDULE_SAGA_ID: &str = "sequence-single-saga";

/// Bypasses `combine` entirely: the effect is hand-built as
/// `Sequence(vec![Persist(domain), Persist(schedule)])`, exercising the public
/// construction path `combine`'s junction fold does not run through.  All
/// `SagaEffect` variants are public, so nothing stops a `Saga::handle`
/// implementation from returning this shape directly.
struct PersistSequencedWithScheduleSaga {
    deadline: Deadline<Reminder>,
    handled: Arc<Notify>,
}

#[async_trait]
impl Saga for PersistSequencedWithScheduleSaga {
    type Subscribe = ScheduleInput;
    type Event = ReservationRequested;
    type Error = std::convert::Infallible;

    fn correlate(input: &Self::Subscribe) -> Option<SagaId> {
        match input {
            ScheduleInput::OrderPlaced(_) => {
                Some(SagaId::new(PERSIST_SEQUENCED_WITH_SCHEDULE_SAGA_ID))
            }
            ScheduleInput::Reminder(_) => None,
        }
    }

    async fn handle(
        &mut self,
        input: Self::Subscribe,
        _ctx: &mut SagaContext,
    ) -> Result<SagaEffect<Self::Event>, Self::Error> {
        match input {
            ScheduleInput::OrderPlaced(event) => {
                let persist = SagaEffect::persist(ReservationRequested {
                    sku: event.sku.clone(),
                });
                let schedule = self.deadline.set(
                    TimerName::new("reservation-deadline"),
                    NEVER_DUE,
                    Reminder { note: event.sku },
                );
                self.handled.notify_one();
                Ok(SagaEffect::Sequence(vec![persist, schedule]))
            }
            ScheduleInput::Reminder(reminder) => Ok(never_due_arm(reminder)),
        }
    }
}

/// Correlation rule of [`PersistSequencedWithEmptyNestedSequenceSaga`]: the
/// single reservation process every `OrderPlaced` in this test belongs to.
const PERSIST_SEQUENCED_WITH_EMPTY_NESTED_SEQUENCE_SAGA_ID: &str = "sequence-empty-nested-saga";

/// Same bypass as [`PersistSequencedWithScheduleSaga`], but with an empty
/// nested `Sequence([])` wedged between the two `Persist` leaves. An empty
/// `Sequence` carries no leaves of its own, so interpreter-side normalization
/// must hoist zero elements from it rather than collapsing it to a
/// `SagaEffect::None` boundary that would split the domain event from its
/// schedule fact.
struct PersistSequencedWithEmptyNestedSequenceSaga {
    deadline: Deadline<Reminder>,
    handled: Arc<Notify>,
}

#[async_trait]
impl Saga for PersistSequencedWithEmptyNestedSequenceSaga {
    type Subscribe = ScheduleInput;
    type Event = ReservationRequested;
    type Error = std::convert::Infallible;

    fn correlate(input: &Self::Subscribe) -> Option<SagaId> {
        match input {
            ScheduleInput::OrderPlaced(_) => Some(SagaId::new(
                PERSIST_SEQUENCED_WITH_EMPTY_NESTED_SEQUENCE_SAGA_ID,
            )),
            ScheduleInput::Reminder(_) => None,
        }
    }

    async fn handle(
        &mut self,
        input: Self::Subscribe,
        _ctx: &mut SagaContext,
    ) -> Result<SagaEffect<Self::Event>, Self::Error> {
        match input {
            ScheduleInput::OrderPlaced(event) => {
                let persist = SagaEffect::persist(ReservationRequested {
                    sku: event.sku.clone(),
                });
                let schedule = self.deadline.set(
                    TimerName::new("reservation-deadline"),
                    NEVER_DUE,
                    Reminder { note: event.sku },
                );
                self.handled.notify_one();
                Ok(SagaEffect::Sequence(vec![
                    persist,
                    SagaEffect::Sequence(vec![]),
                    schedule,
                ]))
            }
            ScheduleInput::Reminder(reminder) => Ok(never_due_arm(reminder)),
        }
    }
}

/// Correlation rule of [`PersistSequencedWithNoneLeafSaga`]: the single
/// reservation process every `OrderPlaced` in this test belongs to.
const PERSIST_SEQUENCED_WITH_NONE_LEAF_SAGA_ID: &str = "sequence-none-leaf-saga";

/// Same bypass as [`PersistSequencedWithScheduleSaga`], but with an explicit
/// `SagaEffect::None` wedged between the two `Persist` leaves. `None` is the
/// Monoid identity, so interpreter-side normalization must drop it rather than
/// let it stand as a merge boundary: an identity element that decides whether
/// the schedule fact shares the domain event's append batch is observable in
/// the store, which is exactly what an identity must never be.
struct PersistSequencedWithNoneLeafSaga {
    deadline: Deadline<Reminder>,
    handled: Arc<Notify>,
}

#[async_trait]
impl Saga for PersistSequencedWithNoneLeafSaga {
    type Subscribe = ScheduleInput;
    type Event = ReservationRequested;
    type Error = std::convert::Infallible;

    fn correlate(input: &Self::Subscribe) -> Option<SagaId> {
        match input {
            ScheduleInput::OrderPlaced(_) => {
                Some(SagaId::new(PERSIST_SEQUENCED_WITH_NONE_LEAF_SAGA_ID))
            }
            ScheduleInput::Reminder(_) => None,
        }
    }

    async fn handle(
        &mut self,
        input: Self::Subscribe,
        _ctx: &mut SagaContext,
    ) -> Result<SagaEffect<Self::Event>, Self::Error> {
        match input {
            ScheduleInput::OrderPlaced(event) => {
                let persist = SagaEffect::persist(ReservationRequested {
                    sku: event.sku.clone(),
                });
                let schedule = self.deadline.set(
                    TimerName::new("reservation-deadline"),
                    NEVER_DUE,
                    Reminder { note: event.sku },
                );
                self.handled.notify_one();
                Ok(SagaEffect::Sequence(vec![
                    persist,
                    SagaEffect::None,
                    schedule,
                ]))
            }
            ScheduleInput::Reminder(reminder) => Ok(never_due_arm(reminder)),
        }
    }
}

/// Correlation rule of [`PersistCombineTwoSchedulesSaga`]: the single
/// reservation process every `OrderPlaced` in this test belongs to.
const PERSIST_COMBINE_TWO_SCHEDULES_SAGA_ID: &str = "combine-order-saga";

/// Two deadlines combined onto one decision, each under a *different* timer
/// name so the merged `schedules` order is observable through the `name` field
/// of the `scheduled` facts.
struct PersistCombineTwoSchedulesSaga {
    deadline: Deadline<Reminder>,
    handled: Arc<Notify>,
}

/// The two timer names [`PersistCombineTwoSchedulesSaga`] sets, in the order it
/// combines them.
const FIRST_TIMER: &str = "reservation-deadline-1";
const SECOND_TIMER: &str = "reservation-deadline-2";

#[async_trait]
impl Saga for PersistCombineTwoSchedulesSaga {
    type Subscribe = ScheduleInput;
    type Event = ReservationRequested;
    type Error = std::convert::Infallible;

    fn correlate(input: &Self::Subscribe) -> Option<SagaId> {
        match input {
            ScheduleInput::OrderPlaced(_) => {
                Some(SagaId::new(PERSIST_COMBINE_TWO_SCHEDULES_SAGA_ID))
            }
            ScheduleInput::Reminder(_) => None,
        }
    }

    async fn handle(
        &mut self,
        input: Self::Subscribe,
        _ctx: &mut SagaContext,
    ) -> Result<SagaEffect<Self::Event>, Self::Error> {
        match input {
            ScheduleInput::OrderPlaced(event) => {
                let effect = SagaEffect::persist(ReservationRequested {
                    sku: event.sku.clone(),
                })
                .combine(self.deadline.set(
                    TimerName::new(FIRST_TIMER),
                    NEVER_DUE,
                    Reminder {
                        note: format!("{}-1", event.sku),
                    },
                ))
                .combine(self.deadline.set(
                    TimerName::new(SECOND_TIMER),
                    NEVER_DUE,
                    Reminder {
                        note: format!("{}-2", event.sku),
                    },
                ));
                self.handled.notify_one();
                Ok(effect)
            }
            ScheduleInput::Reminder(reminder) => Ok(never_due_arm(reminder)),
        }
    }
}

/// Correlation rule of [`PersistCancelSaga`]: the single reservation process
/// every `OrderPlaced` in this test belongs to.
const PERSIST_CANCEL_SAGA_ID: &str = "combine-cancel-saga";

/// A decision combined with a cancellation. `CancelSchedule` carries an
/// interpretation of its own, so it is a hard boundary the `Persist × Persist`
/// fold never crosses — this saga is what makes that boundary observable.
struct PersistCancelSaga {
    deadline: Deadline<Reminder>,
    handled: Arc<Notify>,
}

/// The timer [`PersistCancelSaga`] cancels.  Nothing ever set it: a
/// cancellation writes its fact unconditionally, because the fact is what a
/// later replay reads to *not* re-arm the deadline.
const CANCELLED_TIMER: &str = "reservation-deadline";

#[async_trait]
impl Saga for PersistCancelSaga {
    type Subscribe = ScheduleInput;
    type Event = ReservationRequested;
    type Error = std::convert::Infallible;

    fn correlate(input: &Self::Subscribe) -> Option<SagaId> {
        match input {
            ScheduleInput::OrderPlaced(_) => Some(SagaId::new(PERSIST_CANCEL_SAGA_ID)),
            ScheduleInput::Reminder(_) => None,
        }
    }

    async fn handle(
        &mut self,
        input: Self::Subscribe,
        _ctx: &mut SagaContext,
    ) -> Result<SagaEffect<Self::Event>, Self::Error> {
        match input {
            ScheduleInput::OrderPlaced(event) => {
                let effect = SagaEffect::persist(ReservationRequested { sku: event.sku })
                    .combine(self.deadline.cancel(TimerName::new(CANCELLED_TIMER)));
                self.handled.notify_one();
                Ok(effect)
            }
            ScheduleInput::Reminder(reminder) => Ok(never_due_arm(reminder)),
        }
    }
}

/// Correlation rule of [`PersistThenEndSaga`]: the single reservation process
/// every `OrderPlaced` in this test belongs to.
const PERSIST_THEN_END_SAGA_ID: &str = "combine-end-saga";

/// A decision combined with `End`. `End` writes the durable `Ended` lifecycle
/// fact and stops the process, so it is the other hard boundary the fold never
/// crosses.
struct PersistThenEndSaga {
    handled: Arc<Notify>,
}

#[async_trait]
impl Saga for PersistThenEndSaga {
    type Subscribe = Observed;
    type Event = ReservationRequested;
    type Error = std::convert::Infallible;

    fn correlate(_input: &Self::Subscribe) -> Option<SagaId> {
        Some(SagaId::new(PERSIST_THEN_END_SAGA_ID))
    }

    async fn handle(
        &mut self,
        input: Self::Subscribe,
        _ctx: &mut SagaContext,
    ) -> Result<SagaEffect<Self::Event>, Self::Error> {
        let Observed::OrderPlaced(event) = input;
        self.handled.notify_one();
        Ok(SagaEffect::persist(ReservationRequested { sku: event.sku }).then_end())
    }
}

// Fixtures

async fn append_order_placed(store: &Journal, agg_id: &AggregateId, sequence: u64, sku: &str) {
    let payload = serde_json::to_vec(&OrderPlaced {
        sku: sku.to_owned(),
    })
    .map(Bytes::from)
    .expect("encode OrderPlaced must succeed");
    store
        .append(
            agg_id.as_str(),
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

fn snapshot(batches: &Batches) -> Vec<Vec<RecordedEvent>> {
    batches
        .lock()
        .expect("recording store batch buffer must not be poisoned")
        .clone()
}

/// Poll until the recorded batches hold at least `expected_min` events in
/// total, then return the snapshot.
///
/// Waiting on a *total event count* rather than a batch count keeps the test
/// honest in both directions: a split append and a merged one reach the same
/// total, so a regression either way fails on the batch-grouping assertions
/// instead of timing out.
async fn wait_for_recorded_events(
    batches: &Batches,
    expected_min: usize,
) -> Vec<Vec<RecordedEvent>> {
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    loop {
        let current = snapshot(batches);
        let total: usize = current.iter().map(Vec::len).sum();
        if total >= expected_min {
            return current;
        }
        if std::time::Instant::now() >= deadline {
            panic!(
                "timed out waiting for {expected_min} recorded saga events (had {total}): {:?}",
                describe(&current)
            );
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// The one batch holding the domain event, failing loudly when it was appended
/// zero or more than one time.
fn the_domain_batch(recorded: &[Vec<RecordedEvent>]) -> &Vec<RecordedEvent> {
    let domain_batches: Vec<&Vec<RecordedEvent>> = recorded
        .iter()
        .filter(|batch| batch.iter().any(is_domain_event))
        .collect();
    assert_eq!(
        domain_batches.len(),
        1,
        "the domain event must be appended exactly once; batches: {:?}",
        describe(recorded)
    );
    domain_batches[0]
}

/// Assert that `recorded` holds exactly the one atomic batch
/// `[domain event, scheduled fact, consumed position]` a decision carrying one
/// deadline must produce, whatever `shape` built the effect.
///
/// `shape` names the construction under test so a failure says which route
/// through the algebra broke rather than only that one did.
fn assert_single_atomic_batch(recorded: &[Vec<RecordedEvent>], shape: &str) {
    let atomic_batch = the_domain_batch(recorded);
    assert_eq!(
        atomic_batch.len(),
        3,
        "{shape} must append the domain event, the schedule fact of the deadline \
         it set and the position of the input they were decided from together; \
         batches: {:?}",
        describe(recorded)
    );
    assert!(
        is_domain_event(&atomic_batch[0]),
        "the user event must lead the atomic batch; batches: {:?}",
        describe(recorded)
    );
    assert!(
        is_scheduled_fact(&atomic_batch[1]),
        "the schedule fact must ride in the same batch as the user event, or a \
         crash between two appends could arm a timer against a decision no \
         store holds; batches: {:?}",
        describe(recorded)
    );
    assert!(
        is_consumed_position(&atomic_batch[2]),
        "the position closes the batch, after the rows the effect itself \
         produced; batches: {:?}",
        describe(recorded)
    );

    let scheduled_batches = recorded
        .iter()
        .filter(|batch| batch.iter().any(is_scheduled_fact))
        .count();
    assert_eq!(
        scheduled_batches,
        1,
        "the schedule fact must not be written by an append of its own — the \
         crash window between two appends is exactly what the junction fold \
         closes; batches: {:?}",
        describe(recorded)
    );
}

/// Everything every scenario in this file needs: a runtime, a one-event
/// upstream stream, and a saga store that records the boundary of each append.
///
/// The scenarios differ only in the effect `handle` returns, so the wiring is
/// shared while the spawn — which names the saga type and its producer — and
/// the assertions stay in each test.
struct Fixture {
    ps: ProcessSystem,
    upstream_store: Journal,
    order_id: AggregateId,
    batches: Batches,
    saga_store: Journal,
    handled: Arc<Notify>,
}

async fn fixture(stream: &str, sku: &str) -> Fixture {
    let ps = ProcessSystem::new().await;

    let upstream_store = Journal::from(InMemoryEventStore::default());
    let order_id = AggregateId::new(stream);
    append_order_placed(&upstream_store, &order_id, 1, sku).await;

    let batches: Batches = Arc::new(Mutex::new(Vec::new()));
    let saga_store = Journal::from(RecordingStore::new(Arc::clone(&batches)));

    Fixture {
        ps,
        upstream_store,
        order_id,
        batches,
        saga_store,
        handled: Arc::new(Notify::new()),
    }
}

impl Fixture {
    /// Wait for `handle` to run, then for `expected_min` rows to be recorded,
    /// and answer with the batches the saga's own store observed.
    async fn observe(&self, expected_min: usize) -> Vec<Vec<RecordedEvent>> {
        tokio::time::timeout(Duration::from_secs(3), self.handled.notified())
            .await
            .expect("Saga::handle must run within 3 seconds");
        wait_for_recorded_events(&self.batches, expected_min).await
    }
}

// Tests — the junction folds, however the effect was built

/// Given a saga returning `persist(e).combine(deadline.set(..))`,
/// When the upstream event is handled,
/// Then the domain event, the `scheduled` fact and the consumed position of the
/// input they were decided from are written by a single `store::append` call —
/// a split append could lose the timer's durable record while the event that
/// justifies it survives, or the other way round.
#[tokio::test]
async fn persist_combined_with_schedule_is_written_in_one_append_batch() {
    let f = fixture("combine-schedule-order", "SKU-1").await;

    let handled = Arc::clone(&f.handled);
    let _manager_proxy = SagaManagerProps::<PersistCombineScheduleSaga>::new(
        f.saga_store.clone(),
        move |components| PersistCombineScheduleSaga {
            deadline: components.deadline(),
            handled: Arc::clone(&handled),
        },
    )
    .observing::<OrderPlaced>(Subscription::stream(&f.upstream_store, &f.order_id))
    .spawn(&f.ps)
    .await;

    // domain event + the `scheduled` fact + the consumed position; the timer
    // the host armed is an hour out, so no firing — and so no `fired` fact —
    // reaches the store inside this test.
    let recorded = f.observe(3).await;

    assert_single_atomic_batch(&recorded, "persist(e).combine(deadline.set(..))");
}

/// Given a saga returning the hand-built `Sequence(vec![Persist(e),
/// Persist(schedule)])` — bypassing `combine` entirely,
/// When the upstream event is handled,
/// Then the three rows are still written by a single `store::append` call.
/// `SagaEffect`'s variants are public, so nothing but interpreter-side
/// normalization stops a hand-built `Sequence` from reaching the interpreter
/// unmerged and reopening the window `combine`'s junction fold exists to close.
#[tokio::test]
async fn hand_built_sequence_of_two_persists_is_written_in_one_append_batch() {
    let f = fixture("sequence-single-order", "SKU-SEQ").await;

    let handled = Arc::clone(&f.handled);
    let _manager_proxy = SagaManagerProps::<PersistSequencedWithScheduleSaga>::new(
        f.saga_store.clone(),
        move |components| PersistSequencedWithScheduleSaga {
            deadline: components.deadline(),
            handled: Arc::clone(&handled),
        },
    )
    .observing::<OrderPlaced>(Subscription::stream(&f.upstream_store, &f.order_id))
    .spawn(&f.ps)
    .await;

    let recorded = f.observe(3).await;

    assert_single_atomic_batch(&recorded, "Sequence(vec![Persist(e), Persist(schedule)])");
}

/// Given a saga returning the hand-built `Sequence(vec![Persist(e),
/// Sequence(vec![]), Persist(schedule)])` — an empty nested `Sequence` wedged
/// between the two `Persist` leaves,
/// When the upstream event is handled,
/// Then the three rows are still written by a single `store::append` call. An
/// empty `Sequence` carries zero leaves and must be hoisted away rather than
/// collapsed into a `SagaEffect::None` boundary that would split an
/// otherwise-adjacent `Persist × Persist` junction.
#[tokio::test]
async fn hand_built_sequence_with_empty_nested_sequence_is_written_in_one_append_batch() {
    let f = fixture("sequence-empty-nested-order", "SKU-SEQ-EMPTY").await;

    let handled = Arc::clone(&f.handled);
    let _manager_proxy = SagaManagerProps::<PersistSequencedWithEmptyNestedSequenceSaga>::new(
        f.saga_store.clone(),
        move |components| PersistSequencedWithEmptyNestedSequenceSaga {
            deadline: components.deadline(),
            handled: Arc::clone(&handled),
        },
    )
    .observing::<OrderPlaced>(Subscription::stream(&f.upstream_store, &f.order_id))
    .spawn(&f.ps)
    .await;

    let recorded = f.observe(3).await;

    assert_single_atomic_batch(
        &recorded,
        "Sequence(vec![Persist(e), Sequence(vec![]), Persist(schedule)])",
    );
}

/// Given a saga returning the hand-built `Sequence(vec![Persist(e), None,
/// Persist(schedule)])` — the Monoid identity wedged between the two `Persist`
/// leaves,
/// When the upstream event is handled,
/// Then the three rows are still written by a single `store::append` call.
/// `SagaEffect::None` is a no-op identity; if it survived normalization as a
/// merge boundary, adding an element that interprets to nothing would decide
/// whether a timer is armed against a durable decision.
#[tokio::test]
async fn hand_built_sequence_with_none_leaf_is_written_in_one_append_batch() {
    let f = fixture("sequence-none-leaf-order", "SKU-SEQ-NONE").await;

    let handled = Arc::clone(&f.handled);
    let _manager_proxy = SagaManagerProps::<PersistSequencedWithNoneLeafSaga>::new(
        f.saga_store.clone(),
        move |components| PersistSequencedWithNoneLeafSaga {
            deadline: components.deadline(),
            handled: Arc::clone(&handled),
        },
    )
    .observing::<OrderPlaced>(Subscription::stream(&f.upstream_store, &f.order_id))
    .spawn(&f.ps)
    .await;

    let recorded = f.observe(3).await;

    assert_single_atomic_batch(
        &recorded,
        "Sequence(vec![Persist(e), None, Persist(schedule)])",
    );
}

/// Given a decision combined with two deadlines under different timer names,
/// When the upstream event is handled,
/// Then all four rows land in one append batch with the schedule facts in
/// left-to-right order — the `schedules` concatenation order of the merge is
/// observable here because the stream carries the timer each fact names, while
/// `Shape` can only compare the specs a single effect value holds.
#[tokio::test]
async fn combined_schedules_keep_left_to_right_order_inside_the_single_batch() {
    let f = fixture("combine-order-order", "SKU-2").await;

    let handled = Arc::clone(&f.handled);
    let _manager_proxy = SagaManagerProps::<PersistCombineTwoSchedulesSaga>::new(
        f.saga_store.clone(),
        move |components| PersistCombineTwoSchedulesSaga {
            deadline: components.deadline(),
            handled: Arc::clone(&handled),
        },
    )
    .observing::<OrderPlaced>(Subscription::stream(&f.upstream_store, &f.order_id))
    .spawn(&f.ps)
    .await;

    // domain event + 2 `scheduled` facts + the consumed position.
    let recorded = f.observe(4).await;

    let atomic_batch = the_domain_batch(&recorded);
    assert_eq!(
        atomic_batch.len(),
        4,
        "persist(e).combine(deadline.set(a)).combine(deadline.set(b)) must append \
         the user event, both schedule facts and the consumed position together; \
         batches: {:?}",
        describe(&recorded)
    );
    assert!(
        is_domain_event(&atomic_batch[0]),
        "the user event must lead the atomic batch; batches: {:?}",
        describe(&recorded)
    );
    assert!(
        is_consumed_position(&atomic_batch[3]),
        "the position closes the batch, after every row the effect itself \
         produced; batches: {:?}",
        describe(&recorded)
    );
    assert_eq!(
        scheduled_timer_names(atomic_batch),
        vec![FIRST_TIMER.to_owned(), SECOND_TIMER.to_owned()],
        "merged schedules must keep the left-to-right order of the combine \
         chain; a saga setting two deadlines in one decision owns their order; \
         batches: {:?}",
        describe(&recorded)
    );
}

// Tests — the hard boundaries the fold never crosses

/// Given a saga returning `persist(e).combine(deadline.cancel(..))`,
/// When the upstream event is handled,
/// Then the decision and the cancellation reach the store as **two** appends,
/// and the consumed position rides the second one. `CancelSchedule` carries an
/// interpretation of its own, so folding it into the preceding `Persist` would
/// claim an atomicity the interpreter does not provide; and a position
/// committed with the first append would make "this input is dealt with"
/// durable while the cancellation it also owes is not.
#[tokio::test]
async fn a_cancellation_is_a_hard_boundary_the_fold_does_not_cross() {
    let f = fixture("combine-cancel-order", "SKU-CANCEL").await;

    let handled = Arc::clone(&f.handled);
    let _manager_proxy =
        SagaManagerProps::<PersistCancelSaga>::new(f.saga_store.clone(), move |components| {
            PersistCancelSaga {
                deadline: components.deadline(),
                handled: Arc::clone(&handled),
            }
        })
        .observing::<OrderPlaced>(Subscription::stream(&f.upstream_store, &f.order_id))
        .spawn(&f.ps)
        .await;

    // domain event + the `cancelled` fact + the consumed position.
    let recorded = f.observe(3).await;

    assert_eq!(
        recorded.len(),
        2,
        "a `CancelSchedule` behind a `Persist` must stay its own append; \
         batches: {:?}",
        describe(&recorded)
    );

    let decision = the_domain_batch(&recorded);
    assert_eq!(
        decision.len(),
        1,
        "the decision's append carries the domain event alone — the position \
         belongs to the append that lands last for this input; batches: {:?}",
        describe(&recorded)
    );

    let cancellation = &recorded[1];
    assert_eq!(
        cancellation.len(),
        2,
        "the cancellation's append carries its `cancelled` fact and the position \
         of the input the whole decision was made from; batches: {:?}",
        describe(&recorded)
    );
    assert!(
        is_cancelled_fact(&cancellation[0]),
        "the `cancelled` fact leads its own append; batches: {:?}",
        describe(&recorded)
    );
    assert!(
        is_consumed_position(&cancellation[1]),
        "the position goes on the append that lands last, never an earlier one: \
         recorded sooner it would block the redelivery that would have re-made \
         the cancellation; batches: {:?}",
        describe(&recorded)
    );
}

/// Given a saga returning `persist(e).then_end()`,
/// When the upstream event is handled,
/// Then the decision and the termination reach the store as **two** appends,
/// and the consumed position rides the `Ended` fact's. `End` writes the durable
/// lifecycle fact a later incarnation of the instance reads to refuse running
/// `handle` ever again, so it is a boundary of its own rather than a row folded
/// into the preceding decision.
#[tokio::test]
async fn end_is_a_hard_boundary_the_fold_does_not_cross() {
    let f = fixture("combine-end-order", "SKU-END").await;

    let handled = Arc::clone(&f.handled);
    let _manager_proxy =
        SagaManagerProps::<PersistThenEndSaga>::new(f.saga_store.clone(), move |_components| {
            PersistThenEndSaga {
                handled: Arc::clone(&handled),
            }
        })
        .observing::<OrderPlaced>(Subscription::stream(&f.upstream_store, &f.order_id))
        .spawn(&f.ps)
        .await;

    // domain event + the `Ended` lifecycle fact + the consumed position.
    let recorded = f.observe(3).await;

    assert_eq!(
        recorded.len(),
        2,
        "`End` behind a `Persist` must stay its own append; batches: {:?}",
        describe(&recorded)
    );

    let decision = the_domain_batch(&recorded);
    assert_eq!(
        decision.len(),
        1,
        "the decision's append carries the domain event alone — the position \
         belongs to the append that lands last for this input; batches: {:?}",
        describe(&recorded)
    );

    let termination = &recorded[1];
    assert_eq!(
        termination.len(),
        2,
        "the terminating append carries the `Ended` lifecycle fact and the \
         position of the input the whole decision was made from; batches: {:?}",
        describe(&recorded)
    );
    assert!(
        is_ended_fact(&termination[0]),
        "the `Ended` lifecycle fact leads its own append; batches: {:?}",
        describe(&recorded)
    );
    assert!(
        is_consumed_position(&termination[1]),
        "the position is never durable before the `Ended` fact it depends on is; \
         batches: {:?}",
        describe(&recorded)
    );
}
