//! What an *empty* `Persist` means to the interpreter.
//!
//! `SagaEffect::persist_all(vec![])` keeps a `Persist` variant structurally —
//! it stays a valid `combine` receiver — but a `Persist` whose `events` **and**
//! `schedules` are both empty contributes no batch of its own, exactly like
//! `SagaEffect::None`. Neither is a true no-op, though: the input it was
//! decided from can still have a position pending, and the interpreter writes a
//! consumed-position-only append for it so the input is not redelivered to
//! `Saga::handle`. These tests pin that observable behaviour so the rustdoc on
//! `persist_all` can state it truthfully.
//!
//! The second test pins the other side of the condition, which is the part
//! easiest to get wrong: emptiness of `events` alone is **not** enough. A
//! `Persist` carrying no user events but one schedule is what `Deadline::set`
//! itself builds, and collapsing it to a no-op would silently discard every
//! deadline a saga ever sets — leaving a saga waiting on a timer no stream
//! records and no scheduler ever armed.
//!
//! The first test makes the saga handle a second, event-producing upstream
//! message after the empty one so the batch each input produced can be told
//! apart. Upstream messages are interpreted strictly sequentially (the poller
//! `ask()`s and `recv` awaits the interpreter), so once the second message's
//! batch is recorded the first message's interpretation has already finished
//! and the total batch count is a complete observation.
//!
//! Run with: `cargo test -p nitinol-saga --test saga_empty_persist_is_noop`.

#[path = "common/helpers.rs"]
mod common;
use common::{
    decode_consumed_fact, decode_schedule_kind, JsonCodec, ScheduleKind, CONSUMED_MARKER,
    SCHEDULE_MARKER,
};

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use serde::{Deserialize, Serialize};

use nitinol_eventsource::codec::Codec;
use nitinol_eventsource::system::EventSourceSystem;
use nitinol_eventsource::Event;
use nitinol_persistence::error::{AppendError, LoadError};
use nitinol_persistence::provider::{CustomStore, PersistenceProvider, Unprovided};
use nitinol_persistence::store::{
    EventStore, EventStream, InMemoryEventStore, InMemoryOffsetStore, Journal,
};
use nitinol_persistence::{
    AggregateId, AppendOutcome, AppendingEvent, EventType, Family, LoadQuery, TypeName,
};
use nitinol_runtime::ProcessSystem;
use nitinol_saga::{
    Deadline, DeadlineMessage, Saga, SagaContext, SagaEffect, SagaId, SagaManagerProps,
    SagaSubscribe, Subscription, TimerName,
};

#[derive(Clone, Debug, Serialize, Deserialize)]
struct OrderPlaced {
    sku: String,
}

impl Event for OrderPlaced {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("empty_persist"), TypeName::new("OrderPlaced"));
    type Codec = JsonCodec;
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ReservationRequested {
    sku: String,
}

impl Event for ReservationRequested {
    const EVENT_TYPE: EventType = EventType::new(
        Family::new("empty_persist"),
        TypeName::new("ReservationRequested"),
    );
    type Codec = JsonCodec;
}

/// The value the schedule-only saga promises its future self. Persisted inside
/// the schedule fact rather than as a row of its own, so it names the `Codec`
/// that writes it down.
#[derive(Clone, Debug, PartialEq)]
struct Reminder {
    note: String,
}

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

#[derive(Clone, Debug)]
struct RecordedEvent {
    event_type: EventType,
    payload: Bytes,
}

type Batches = Arc<Mutex<Vec<Vec<RecordedEvent>>>>;

/// `EventStore` decorator recording one entry per successful `append` call to
/// one named stream.
///
/// The stream is named because a system distributes one journal: the upstream
/// rows a test seeds and the saga's own stream are tenants of it, and a batch
/// boundary the test seeded is not one the interpreter produced.
///
/// Kept local rather than shared with `saga_combine_persist_single_append.rs`:
/// that file is the standing evidence for the #69 atomic-batch contract and
/// owns its own copy for the same reason.
#[derive(Clone)]
struct RecordingStore {
    inner: InMemoryEventStore,
    /// Stream key whose appends are recorded.
    watched: String,
    batches: Batches,
}

impl RecordingStore {
    fn watching(saga_id: &str, batches: Batches) -> Self {
        Self {
            inner: InMemoryEventStore::default(),
            watched: saga_id.to_owned(),
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
        let watched = key == self.watched;
        // Record only after the append durably succeeds: a failed attempt never
        // produced a batch boundary in the stream.
        let outcome = self.inner.append(key, events).await?;
        if watched {
            self.batches
                .lock()
                .expect("recording store batch buffer must not be poisoned")
                .push(recorded);
        }
        Ok(outcome)
    }

    async fn load(&self, query: LoadQuery) -> Result<EventStream<'_>, LoadError> {
        self.inner.load(query).await
    }
}

/// The store a saga is spawned onto here: the journal under observation plus
/// the saga-offset slot, and no other slot — nothing in this file snapshots or
/// projects.
type RecordingSagaStore = CustomStore<Unprovided, Unprovided, InMemoryOffsetStore>;

/// A system over `journal` and the slot the delivery host every saga spawn
/// starts records its progress in.
async fn system_over(journal: &RecordingStore) -> EventSourceSystem<RecordingSagaStore> {
    EventSourceSystem::builder(ProcessSystem::new().await)
        .with_persistence(PersistenceProvider::new(
            CustomStore::builder()
                .eventstore(journal.clone())
                .saga_offsets(InMemoryOffsetStore::default())
                .build(),
        ))
        .build()
}

fn is_domain_event(event: &RecordedEvent) -> bool {
    event.event_type == ReservationRequested::EVENT_TYPE
}

/// The transition a schedule row records, or `None` when the row is not one.
///
/// `nitinol.saga.schedule` is a single identity whose payload `oneof` carries
/// the transition, so a `scheduled` fact is only distinguishable by decoding.
fn schedule_kind(event: &RecordedEvent) -> Option<ScheduleKind> {
    (event.event_type == SCHEDULE_MARKER).then(|| decode_schedule_kind(&event.payload))
}

fn is_scheduled_fact(event: &RecordedEvent) -> bool {
    matches!(schedule_kind(event), Some(ScheduleKind::Scheduled(_)))
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
    event.event_type.to_string()
}

fn describe(batches: &[Vec<RecordedEvent>]) -> Vec<Vec<String>> {
    batches
        .iter()
        .map(|batch| batch.iter().map(describe_row).collect())
        .collect()
}

fn snapshot(batches: &Batches) -> Vec<Vec<RecordedEvent>> {
    batches
        .lock()
        .expect("recording store batch buffer must not be poisoned")
        .clone()
}

/// The sku whose `handle` returns the all-empty `Persist`.
const EMPTY_SKU: &str = "SKU-EMPTY";
/// The sku whose `handle` returns a real domain event, used as the barrier that
/// proves the empty one was already interpreted.
const REAL_SKU: &str = "SKU-REAL";

/// Poll until a batch containing the domain event has been recorded.
///
/// Waiting on the *barrier* event rather than on a batch count keeps the test
/// honest: were the empty `Persist` to append something, the extra batch would
/// already be recorded by the time this returns, and the count assertion —
/// not a timeout — is what reports it.
async fn wait_for_domain_batch(batches: &Batches) -> Vec<Vec<RecordedEvent>> {
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    loop {
        let current = snapshot(batches);
        if current
            .iter()
            .any(|batch| batch.iter().any(is_domain_event))
        {
            return current;
        }
        if std::time::Instant::now() >= deadline {
            panic!(
                "timed out waiting for the barrier domain event: {:?}",
                describe(&current)
            );
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

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

/// The alphabet [`EmptyThenRealSaga`] reacts to: one observed source, no
/// deadline — it has no use for one.
#[derive(SagaSubscribe)]
enum Observed {
    Order(OrderPlaced),
}

/// Returns `persist_all(vec![])` for [`EMPTY_SKU`] and a real event otherwise.
struct EmptyThenRealSaga {
    handled: Arc<AtomicUsize>,
}

const EMPTY_THEN_REAL_SAGA_ID: &str = "empty-then-real-saga";

#[async_trait]
impl Saga for EmptyThenRealSaga {
    type Subscribe = Observed;
    type Event = ReservationRequested;
    type Error = std::convert::Infallible;

    fn correlate(_input: &Self::Subscribe) -> Option<SagaId> {
        Some(SagaId::new(EMPTY_THEN_REAL_SAGA_ID))
    }

    async fn handle(
        &mut self,
        input: Self::Subscribe,
        _ctx: &mut SagaContext,
    ) -> Result<SagaEffect<Self::Event>, Self::Error> {
        let Observed::Order(event) = input;
        self.handled.fetch_add(1, Ordering::SeqCst);
        Ok(if event.sku == EMPTY_SKU {
            SagaEffect::persist_all(vec![])
        } else {
            SagaEffect::persist(ReservationRequested { sku: event.sku })
        })
    }
}

/// The alphabet [`ScheduleOnlySaga`] reacts to: one observed source, and the
/// saga's own time. Declaring the deadline variant is what lets its producer
/// ask `SagaComponents::deadline` for a `Deadline<Reminder>`, and it is the
/// whole of the scheduler wiring.
#[derive(SagaSubscribe)]
enum ScheduleInput {
    Order(OrderPlaced),
    #[subscribe(deadline)]
    Reminder(Reminder),
}

/// The timer [`ScheduleOnlySaga`] sets. An hour out, so the `scheduled` fact is
/// the only row the deadline ever contributes inside this test.
const SCHEDULE_ONLY_TIMER: &str = "reservation-deadline";
const NEVER_DUE: Duration = Duration::from_secs(3600);

/// Returns a bare deadline: a `Persist` with no user events but one schedule.
struct ScheduleOnlySaga {
    deadline: Deadline<Reminder>,
    handled: Arc<AtomicUsize>,
}

const SCHEDULE_ONLY_SAGA_ID: &str = "schedule-only-saga";

#[async_trait]
impl Saga for ScheduleOnlySaga {
    type Subscribe = ScheduleInput;
    type Event = ReservationRequested;
    type Error = std::convert::Infallible;

    fn correlate(input: &Self::Subscribe) -> Option<SagaId> {
        match input {
            ScheduleInput::Order(_) => Some(SagaId::new(SCHEDULE_ONLY_SAGA_ID)),
            // A deadline firing never travels the subscription path.
            ScheduleInput::Reminder(_) => None,
        }
    }

    async fn handle(
        &mut self,
        input: Self::Subscribe,
        _ctx: &mut SagaContext,
    ) -> Result<SagaEffect<Self::Event>, Self::Error> {
        match input {
            ScheduleInput::Order(event) => {
                self.handled.fetch_add(1, Ordering::SeqCst);
                Ok(self.deadline.set(
                    TimerName::new(SCHEDULE_ONLY_TIMER),
                    NEVER_DUE,
                    Reminder { note: event.sku },
                ))
            }
            ScheduleInput::Reminder(reminder) => {
                // The deadline is an hour out, so no scenario in this file
                // reaches here. Naming the note keeps the firing's payload part
                // of the alphabet rather than a field nothing ever reads, and
                // the arm writes nothing so a firing could not disturb the
                // batch assertions below.
                let _never_due = reminder.note;
                Ok(SagaEffect::None)
            }
        }
    }
}

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

/// Given a saga that returns `persist_all(vec![])` — a `Persist` whose events
/// and schedules are both empty — followed by one that returns a real event,
/// When both upstream events are handled,
/// Then the store receives one `append` call per input: the empty `Persist`
/// carries nothing of its own, exactly like `SagaEffect::None`, but its
/// append still carries the consumed position of the input it was decided
/// from; the real event's append carries the domain event and its own
/// position.
#[tokio::test]
async fn all_empty_persist_writes_only_the_consumed_position() {
    let batches: Batches = Arc::new(Mutex::new(Vec::new()));
    let journal = RecordingStore::watching(EMPTY_THEN_REAL_SAGA_ID, Arc::clone(&batches));
    let system = system_over(&journal).await;

    let order_id = AggregateId::new("empty-persist-order");
    let seeding = system.persistence().event_store();
    append_order_placed(&seeding, &order_id, 1, EMPTY_SKU).await;
    append_order_placed(&seeding, &order_id, 2, REAL_SKU).await;

    let handled = Arc::new(AtomicUsize::new(0));

    let handled_clone = Arc::clone(&handled);
    let _host = SagaManagerProps::<EmptyThenRealSaga>::new(move |_components| EmptyThenRealSaga {
        handled: Arc::clone(&handled_clone),
    })
    .observing::<OrderPlaced>(Subscription::stream(&order_id))
    .spawn(&system)
    .await;

    let recorded = wait_for_domain_batch(&batches).await;

    assert_eq!(
        handled.load(Ordering::SeqCst),
        2,
        "both upstream events must have reached Saga::handle before the store \
         observation is complete; batches: {:?}",
        describe(&recorded)
    );

    assert_eq!(
        recorded.len(),
        2,
        "each input gets its own append — the empty Persist's position-only \
         append, and the real event's append; batches: {:?}",
        describe(&recorded)
    );
    assert_eq!(
        recorded[0].len(),
        1,
        "the empty Persist contributes nothing of its own; the single row in \
         its append is the consumed position of the input it was decided \
         from; batches: {:?}",
        describe(&recorded)
    );
    assert_eq!(
        recorded[0][0].event_type,
        CONSUMED_MARKER,
        "the only row the empty Persist's append carries is the consumed \
         position of the input it was decided from; batches: {:?}",
        describe(&recorded)
    );
    assert_eq!(
        recorded[1].len(),
        2,
        "the real event's append must carry the domain event and the position \
         of the input it was decided from, and nothing else; batches: {:?}",
        describe(&recorded)
    );
    assert!(
        is_domain_event(&recorded[1][0]),
        "the first row of the real event's append must be the domain event; \
         batches: {:?}",
        describe(&recorded)
    );
    assert_eq!(
        recorded[1][1].event_type,
        CONSUMED_MARKER,
        "the position of the input it was decided from closes the batch; \
         batches: {:?}",
        describe(&recorded)
    );
}

/// Given a saga returning a bare deadline — a `Persist` with no user events but
/// one schedule, which is exactly what `Deadline::set` builds,
/// When the upstream event is handled,
/// Then the `scheduled` fact is appended: emptiness of `events` alone must
/// never make a `Persist` a no-op, or every deadline a saga sets would be
/// silently dropped and the saga would wait on a timer nothing ever armed.
#[tokio::test]
async fn persist_with_no_events_but_a_schedule_is_not_a_noop() {
    let batches: Batches = Arc::new(Mutex::new(Vec::new()));
    let journal = RecordingStore::watching(SCHEDULE_ONLY_SAGA_ID, Arc::clone(&batches));
    let system = system_over(&journal).await;

    let order_id = AggregateId::new("schedule-only-order");
    append_order_placed(
        &system.persistence().event_store(),
        &order_id,
        1,
        "SKU-SCHEDULE",
    )
    .await;

    let handled = Arc::new(AtomicUsize::new(0));

    let handled_clone = Arc::clone(&handled);
    let _host = SagaManagerProps::<ScheduleOnlySaga>::new(move |components| ScheduleOnlySaga {
        deadline: components.deadline(),
        handled: Arc::clone(&handled_clone),
    })
    .observing::<OrderPlaced>(Subscription::stream(&order_id))
    .spawn(&system)
    .await;

    // The `scheduled` fact and the consumed position of the input it was
    // decided from, in the one atomic append the branch produces.
    let recorded = wait_for_recorded_events(&batches, 2).await;

    assert_eq!(
        recorded.len(),
        1,
        "the branch is one `Persist`, so it reaches the store as one append; \
         batches: {:?}",
        describe(&recorded)
    );
    assert_eq!(
        recorded[0].len(),
        2,
        "the append carries the schedule fact and the position of the input it \
         was decided from, and nothing else; batches: {:?}",
        describe(&recorded)
    );
    assert!(
        is_scheduled_fact(&recorded[0][0]),
        "a Persist carrying a schedule but no user events must still be \
         interpreted — the `scheduled` fact must reach the store, or the \
         deadline is lost without a trace; batches: {:?}",
        describe(&recorded)
    );
    assert_eq!(
        recorded[0][1].event_type,
        CONSUMED_MARKER,
        "the position closes the batch, after the row the effect itself \
         produced; batches: {:?}",
        describe(&recorded)
    );
    assert!(
        !recorded
            .iter()
            .any(|batch| batch.iter().any(is_domain_event)),
        "the saga persisted no user event, so no domain event may appear; \
         batches: {:?}",
        describe(&recorded)
    );
}
