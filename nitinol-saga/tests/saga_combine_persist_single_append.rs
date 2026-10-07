//! The `persist(e).combine(tell(..))` route must reach the saga's event store
//! as **one** `store::append` call.
//!
//! The domain event and the `TellRequested` outbox marker occupy consecutive
//! stream sequences either way, so sequence numbers alone cannot tell "one
//! atomic append of two events" apart from "two appends of one event each".
//! The batch boundary of each `append` call is therefore recorded directly by a
//! decorating [`RecordingStore`]; that grouping is the only observation that
//! distinguishes the merged behaviour from the at-most-once tell-loss window a
//! split append opens (a crash between the two appends drops the marker, so
//! replay never re-dispatches the tell).

#[path = "common/helpers.rs"]
mod common;
use common::{
    decode_outbox_kind, decode_schedule_kind, JsonCodec, OutboxKind, ScheduleKind,
    CONSUMED_POSITION_MARKER, OUTBOX_MARKER, SCHEDULE_MARKER,
};

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use serde::{Deserialize, Serialize};
use tokio::sync::Notify;

use nitinol_eventsource::codec::Codec;
use nitinol_eventsource::{
    system::EventSourceSystem, Aggregate, AggregateProxy, Decider, Decision, Event,
};
use nitinol_persistence::error::{AppendError, LoadError};
use nitinol_persistence::provider::PersistenceProvider;
use nitinol_persistence::store::{EventStore, EventStream, InMemoryEventStore, InMemoryStore};
use nitinol_persistence::{
    AggregateId, AppendOutcome, AppendingEvent, EventType, Family, LoadQuery, TypeName,
};
use nitinol_runtime::ProcessSystem;
use nitinol_saga::{
    Deadline, DeadlineMessage, Saga, SagaContext, SagaEffect, SagaId, SagaProps, SagaSubscribe,
    Subscription, TimerName,
};

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

#[derive(Clone, Debug, Serialize, Deserialize)]
struct InventoryReserved {
    sku: String,
}

impl Event for InventoryReserved {
    const EVENT_TYPE: EventType = EventType::new(
        Family::new("combine_atomic"),
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

#[derive(Clone, Debug)]
struct RecordedEvent {
    event_type: EventType,
    payload: Bytes,
}

type Batches = Arc<Mutex<Vec<Vec<RecordedEvent>>>>;

/// `EventStore` decorator that appends one entry to `batches` per successful
/// `append` call, preserving the events of that call as a group.
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

fn outbox_kind(event: &RecordedEvent) -> Option<OutboxKind> {
    (event.event_type == OUTBOX_MARKER).then(|| decode_outbox_kind(&event.payload))
}

fn is_tell_requested(event: &RecordedEvent) -> bool {
    matches!(outbox_kind(event), Some(OutboxKind::TellRequested(_)))
}

/// Target stream keys of the `TellRequested` markers inside `batch`, in the
/// order they were appended.
fn tell_targets(batch: &[RecordedEvent]) -> Vec<String> {
    batch
        .iter()
        .filter_map(|e| match outbox_kind(e) {
            Some(OutboxKind::TellRequested(payload)) => Some(payload.target),
            _ => None,
        })
        .collect()
}

/// The position of the upstream input the batch was decided from — framework
/// bookkeeping that rides the append closing that input out, which for every
/// saga in this file is the single batch its one `Persist` becomes.
fn is_consumed_position(event: &RecordedEvent) -> bool {
    event.event_type == CONSUMED_POSITION_MARKER
}

fn is_scheduled_marker(event: &RecordedEvent) -> bool {
    event.event_type == SCHEDULE_MARKER
        && matches!(
            decode_schedule_kind(&event.payload),
            ScheduleKind::Scheduled(_)
        )
}

fn describe(batches: &[Vec<RecordedEvent>]) -> Vec<Vec<String>> {
    batches
        .iter()
        .map(|batch| {
            batch
                .iter()
                .map(|e| match outbox_kind(e) {
                    Some(OutboxKind::TellRequested(p)) => format!("TellRequested({})", p.target),
                    Some(OutboxKind::TellAcked(_)) => "TellAcked".to_owned(),
                    Some(OutboxKind::TellFailed(_)) => "TellFailed".to_owned(),
                    Some(OutboxKind::Scheduled(_)) => "Scheduled".to_owned(),
                    Some(OutboxKind::Ended(_)) => "Ended".to_owned(),
                    None => e.event_type.to_string(),
                })
                .collect()
        })
        .collect()
}

/// The alphabet every saga in this file but [`PersistCombineScheduleSaga`]
/// reacts to: one observed source, no deadline.
#[derive(SagaSubscribe)]
enum Observed {
    OrderPlaced(OrderPlaced),
}

/// Correlation rule of [`PersistCombineTellSaga`]: the single reservation
/// process every `OrderPlaced` in this test belongs to.
const PERSIST_COMBINE_TELL_SAGA_ID: &str = "combine-single-saga";

/// Mirrors `examples/saga/1.basic-saga/src/saga.rs` verbatim in shape:
/// `persist(..).combine(tell(..))`.
struct PersistCombineTellSaga {
    inventory: AggregateProxy<Inventory>,
    handled: Arc<Notify>,
}

#[async_trait]
impl Saga for PersistCombineTellSaga {
    type Subscribe = Observed;
    type Event = ReservationRequested;
    type Error = std::convert::Infallible;

    fn correlate(_input: &Self::Subscribe) -> Option<SagaId> {
        Some(SagaId::new(PERSIST_COMBINE_TELL_SAGA_ID))
    }

    async fn handle(
        &mut self,
        input: Self::Subscribe,
        _ctx: &mut SagaContext,
    ) -> Result<SagaEffect<Self::Event>, Self::Error> {
        let Observed::OrderPlaced(event) = input;
        let persist = SagaEffect::persist(ReservationRequested {
            sku: event.sku.clone(),
        });
        let tell = SagaEffect::tell(self.inventory.clone(), Reserve { sku: event.sku });
        self.handled.notify_one();
        Ok(persist.combine(tell))
    }
}

/// Bypasses `combine` entirely: the effect is hand-built as
/// `Sequence(vec![Persist(domain), Persist(tell)])`, exercising the public
/// construction path `combine`'s junction merge does not run through.  All
/// `SagaEffect` variants are public, so nothing stops a `Saga::handle`
/// implementation from returning this shape directly.
struct PersistSequencedWithTellSaga {
    inventory: AggregateProxy<Inventory>,
    handled: Arc<Notify>,
}

/// Correlation rule of [`PersistSequencedWithTellSaga`]: the single reservation
/// process every `OrderPlaced` in this test belongs to.
const PERSIST_SEQUENCED_WITH_TELL_SAGA_ID: &str = "sequence-single-saga";

#[async_trait]
impl Saga for PersistSequencedWithTellSaga {
    type Subscribe = Observed;
    type Event = ReservationRequested;
    type Error = std::convert::Infallible;

    fn correlate(_input: &Self::Subscribe) -> Option<SagaId> {
        Some(SagaId::new(PERSIST_SEQUENCED_WITH_TELL_SAGA_ID))
    }

    async fn handle(
        &mut self,
        input: Self::Subscribe,
        _ctx: &mut SagaContext,
    ) -> Result<SagaEffect<Self::Event>, Self::Error> {
        let Observed::OrderPlaced(event) = input;
        let persist = SagaEffect::persist(ReservationRequested {
            sku: event.sku.clone(),
        });
        let tell = SagaEffect::tell(self.inventory.clone(), Reserve { sku: event.sku });
        self.handled.notify_one();
        Ok(SagaEffect::Sequence(vec![persist, tell]))
    }
}

/// Same bypass as `PersistSequencedWithTellSaga`, but with an empty nested
/// `Sequence([])` wedged between the two `Persist` leaves —
/// `Sequence(vec![Persist(domain), Sequence(vec![]), Persist(tell)])`. An
/// empty `Sequence` carries no leaves of its own, so interpreter-side
/// normalization must hoist zero elements from it rather than collapsing it
/// to a `SagaEffect::None` boundary that would split the domain event from
/// its `TellRequested` marker.
struct PersistSequencedWithEmptyNestedSequenceSaga {
    inventory: AggregateProxy<Inventory>,
    handled: Arc<Notify>,
}

/// Correlation rule of [`PersistSequencedWithEmptyNestedSequenceSaga`]: the
/// single reservation process every `OrderPlaced` in this test belongs to.
const PERSIST_SEQUENCED_WITH_EMPTY_NESTED_SEQUENCE_SAGA_ID: &str = "sequence-empty-nested-saga";

#[async_trait]
impl Saga for PersistSequencedWithEmptyNestedSequenceSaga {
    type Subscribe = Observed;
    type Event = ReservationRequested;
    type Error = std::convert::Infallible;

    fn correlate(_input: &Self::Subscribe) -> Option<SagaId> {
        Some(SagaId::new(
            PERSIST_SEQUENCED_WITH_EMPTY_NESTED_SEQUENCE_SAGA_ID,
        ))
    }

    async fn handle(
        &mut self,
        input: Self::Subscribe,
        _ctx: &mut SagaContext,
    ) -> Result<SagaEffect<Self::Event>, Self::Error> {
        let Observed::OrderPlaced(event) = input;
        let persist = SagaEffect::persist(ReservationRequested {
            sku: event.sku.clone(),
        });
        let tell = SagaEffect::tell(self.inventory.clone(), Reserve { sku: event.sku });
        self.handled.notify_one();
        Ok(SagaEffect::Sequence(vec![
            persist,
            SagaEffect::Sequence(vec![]),
            tell,
        ]))
    }
}

/// Same bypass as `PersistSequencedWithTellSaga`, but with an explicit
/// `SagaEffect::None` wedged between the two `Persist` leaves —
/// `Sequence(vec![Persist(domain), None, Persist(tell)])`. `None` is the Monoid
/// identity, so interpreter-side normalization must drop it rather than let it
/// stand as a merge boundary: an identity element that decides whether the tell
/// marker shares the domain event's append batch is observable in the store.
struct PersistSequencedWithNoneLeafSaga {
    inventory: AggregateProxy<Inventory>,
    handled: Arc<Notify>,
}

/// Correlation rule of [`PersistSequencedWithNoneLeafSaga`]: the single
/// reservation process every `OrderPlaced` in this test belongs to.
const PERSIST_SEQUENCED_WITH_NONE_LEAF_SAGA_ID: &str = "sequence-none-leaf-saga";

#[async_trait]
impl Saga for PersistSequencedWithNoneLeafSaga {
    type Subscribe = Observed;
    type Event = ReservationRequested;
    type Error = std::convert::Infallible;

    fn correlate(_input: &Self::Subscribe) -> Option<SagaId> {
        Some(SagaId::new(PERSIST_SEQUENCED_WITH_NONE_LEAF_SAGA_ID))
    }

    async fn handle(
        &mut self,
        input: Self::Subscribe,
        _ctx: &mut SagaContext,
    ) -> Result<SagaEffect<Self::Event>, Self::Error> {
        let Observed::OrderPlaced(event) = input;
        let persist = SagaEffect::persist(ReservationRequested {
            sku: event.sku.clone(),
        });
        let tell = SagaEffect::tell(self.inventory.clone(), Reserve { sku: event.sku });
        self.handled.notify_one();
        Ok(SagaEffect::Sequence(vec![persist, SagaEffect::None, tell]))
    }
}

/// Two tells combined onto one persist, each aimed at a *different* aggregate
/// so the merged `tells` order is observable through the `target` field of the
/// `TellRequested` markers.
struct PersistCombineTwoTellsSaga {
    first: AggregateProxy<Inventory>,
    second: AggregateProxy<Inventory>,
    handled: Arc<Notify>,
}

/// Correlation rule of [`PersistCombineTwoTellsSaga`]: the single reservation
/// process every `OrderPlaced` in this test belongs to.
const PERSIST_COMBINE_TWO_TELLS_SAGA_ID: &str = "combine-order-saga";

#[async_trait]
impl Saga for PersistCombineTwoTellsSaga {
    type Subscribe = Observed;
    type Event = ReservationRequested;
    type Error = std::convert::Infallible;

    fn correlate(_input: &Self::Subscribe) -> Option<SagaId> {
        Some(SagaId::new(PERSIST_COMBINE_TWO_TELLS_SAGA_ID))
    }

    async fn handle(
        &mut self,
        input: Self::Subscribe,
        _ctx: &mut SagaContext,
    ) -> Result<SagaEffect<Self::Event>, Self::Error> {
        let Observed::OrderPlaced(event) = input;
        let effect = SagaEffect::persist(ReservationRequested {
            sku: event.sku.clone(),
        })
        .combine(SagaEffect::tell(
            self.first.clone(),
            Reserve {
                sku: format!("{}-1", event.sku),
            },
        ))
        .combine(SagaEffect::tell(
            self.second.clone(),
            Reserve {
                sku: format!("{}-2", event.sku),
            },
        ));
        self.handled.notify_one();
        Ok(effect)
    }
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
    type Codec = ReminderCodec;
}

/// The alphabet of [`PersistCombineScheduleSaga`]: the one observed source, and
/// the saga's own time.  Declaring the deadline variant is the whole of the
/// scheduler wiring — the host resolves its resident timer core from it.
#[derive(SagaSubscribe)]
enum ScheduleInput {
    OrderPlaced(OrderPlaced),
    #[subscribe(deadline)]
    Reminder(Reminder),
}

/// A persist combined with a deadline — the third category the merge
/// concatenates.  The deadline is set an hour out, so within this test only the
/// `scheduled` marker ever reaches the store; the durable batch boundary it
/// shares with the domain event is what this saga exists to expose.
struct PersistCombineScheduleSaga {
    deadline: Deadline<Reminder>,
    handled: Arc<Notify>,
}

/// Correlation rule of [`PersistCombineScheduleSaga`]: the single reservation
/// process every `OrderPlaced` in this test belongs to.  A deadline firing
/// never travels the subscription path, so its arm answers `None`.
const PERSIST_COMBINE_SCHEDULE_SAGA_ID: &str = "combine-schedule-saga";

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
                    Duration::from_secs(3600),
                    Reminder {
                        note: event.sku.clone(),
                    },
                ));
                self.handled.notify_one();
                Ok(effect)
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

async fn append_order_placed(
    store: &Arc<dyn EventStore>,
    agg_id: &AggregateId,
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
/// honest: the pre-fix split-append behaviour reaches the same total, so a
/// regression fails on the batch-grouping assertions instead of timing out.
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

/// Given a saga returning `persist(e).combine(tell(..))`,
/// When the upstream event is handled,
/// Then the domain event and its `TellRequested` marker are written by a single
/// `store::append` call — closing the at-most-once tell-loss window.
#[tokio::test]
async fn persist_combined_with_tell_is_written_in_one_append_batch() {
    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::builder(ps)
        .with_persistence(PersistenceProvider::new(InMemoryStore::default()))
        .build();

    let upstream_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let order_id = AggregateId::new("combine-single-order");
    append_order_placed(&upstream_store, &order_id, 1, "SKU-1").await;

    let inventory = system
        .spawn_aggregate::<Inventory>(AggregateId::new("combine-single-inventory"))
        .await;

    let batches: Batches = Arc::new(Mutex::new(Vec::new()));
    let saga_store: Arc<dyn EventStore> = Arc::new(RecordingStore::new(Arc::clone(&batches)));
    let saga_id = SagaId::new(PERSIST_COMBINE_TELL_SAGA_ID);
    let handled = Arc::new(Notify::new());

    let inventory_clone = inventory.clone();
    let handled_clone = Arc::clone(&handled);

    let _saga_proxy = SagaProps::<PersistCombineTellSaga>::new(
        saga_id.clone(),
        Arc::clone(&saga_store),
        move |_components| PersistCombineTellSaga {
            inventory: inventory_clone.clone(),
            handled: Arc::clone(&handled_clone),
        },
    )
    .subscribed_to::<OrderPlaced>(Subscription::stream(&upstream_store, &order_id))
    .spawn(system.process_system())
    .await;

    tokio::time::timeout(Duration::from_secs(3), handled.notified())
        .await
        .expect("Saga::handle must run within 3 seconds");

    // domain event + TellRequested + consumed position + the eventual TellAcked.
    let recorded = wait_for_recorded_events(&batches, 4).await;

    let domain_batches: Vec<&Vec<RecordedEvent>> = recorded
        .iter()
        .filter(|batch| batch.iter().any(is_domain_event))
        .collect();
    assert_eq!(
        domain_batches.len(),
        1,
        "the domain event must be appended exactly once; batches: {:?}",
        describe(&recorded)
    );

    let atomic_batch = domain_batches[0];
    assert_eq!(
        atomic_batch.len(),
        3,
        "persist(e).combine(tell(..)) must append the domain event, the \
         TellRequested marker and the position of the input they were decided \
         from together; batches: {:?}",
        describe(&recorded)
    );
    assert!(
        is_domain_event(&atomic_batch[0]),
        "the user event must lead the atomic batch; batches: {:?}",
        describe(&recorded)
    );
    assert!(
        is_tell_requested(&atomic_batch[1]),
        "the TellRequested marker must ride in the same batch as the user event; \
         batches: {:?}",
        describe(&recorded)
    );
    assert!(
        is_consumed_position(&atomic_batch[2]),
        "the position closes the batch, so the marker before it keeps the \
         sequence its tell_id names; batches: {:?}",
        describe(&recorded)
    );

    let tell_requested_batches = recorded
        .iter()
        .filter(|batch| batch.iter().any(is_tell_requested))
        .count();
    assert_eq!(
        tell_requested_batches,
        1,
        "the TellRequested marker must not be written by an append of its own — \
         a crash between the two appends is exactly the tell-loss window the merge closes; \
         batches: {:?}",
        describe(&recorded)
    );
}

/// Given a saga returning the hand-built `Sequence(vec![Persist(e),
/// Persist(tell)])` — bypassing `combine` entirely,
/// When the upstream event is handled,
/// Then the domain event and its `TellRequested` marker are still written by a
/// single `store::append` call.  `SagaEffect`'s variants are public, so
/// nothing but interpreter-side normalization stops a hand-built `Sequence`
/// from reaching the interpreter unmerged and reopening the tell-loss window
/// `combine`'s junction merge exists to close.
#[tokio::test]
async fn hand_built_sequence_of_two_persists_is_written_in_one_append_batch() {
    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::builder(ps)
        .with_persistence(PersistenceProvider::new(InMemoryStore::default()))
        .build();

    let upstream_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let order_id = AggregateId::new("sequence-single-order");
    append_order_placed(&upstream_store, &order_id, 1, "SKU-SEQ").await;

    let inventory = system
        .spawn_aggregate::<Inventory>(AggregateId::new("sequence-single-inventory"))
        .await;

    let batches: Batches = Arc::new(Mutex::new(Vec::new()));
    let saga_store: Arc<dyn EventStore> = Arc::new(RecordingStore::new(Arc::clone(&batches)));
    let saga_id = SagaId::new(PERSIST_SEQUENCED_WITH_TELL_SAGA_ID);
    let handled = Arc::new(Notify::new());

    let inventory_clone = inventory.clone();
    let handled_clone = Arc::clone(&handled);

    let _saga_proxy = SagaProps::<PersistSequencedWithTellSaga>::new(
        saga_id.clone(),
        Arc::clone(&saga_store),
        move |_components| PersistSequencedWithTellSaga {
            inventory: inventory_clone.clone(),
            handled: Arc::clone(&handled_clone),
        },
    )
    .subscribed_to::<OrderPlaced>(Subscription::stream(&upstream_store, &order_id))
    .spawn(system.process_system())
    .await;

    tokio::time::timeout(Duration::from_secs(3), handled.notified())
        .await
        .expect("Saga::handle must run within 3 seconds");

    // domain event + TellRequested + consumed position + the eventual TellAcked.
    let recorded = wait_for_recorded_events(&batches, 4).await;

    let domain_batches: Vec<&Vec<RecordedEvent>> = recorded
        .iter()
        .filter(|batch| batch.iter().any(is_domain_event))
        .collect();
    assert_eq!(
        domain_batches.len(),
        1,
        "the domain event must be appended exactly once; batches: {:?}",
        describe(&recorded)
    );

    let atomic_batch = domain_batches[0];
    assert_eq!(
        atomic_batch.len(),
        3,
        "Sequence(vec![Persist(e), Persist(tell)]) must append the domain event, \
         the TellRequested marker and the consumed position together, exactly \
         like persist(e).combine(tell(..)); batches: {:?}",
        describe(&recorded)
    );
    assert!(
        is_domain_event(&atomic_batch[0]),
        "the user event must lead the atomic batch; batches: {:?}",
        describe(&recorded)
    );
    assert!(
        is_tell_requested(&atomic_batch[1]),
        "the TellRequested marker must ride in the same batch as the user event; \
         batches: {:?}",
        describe(&recorded)
    );
    assert!(
        is_consumed_position(&atomic_batch[2]),
        "the position closes the batch, so the marker before it keeps the \
         sequence its tell_id names; batches: {:?}",
        describe(&recorded)
    );

    let tell_requested_batches = recorded
        .iter()
        .filter(|batch| batch.iter().any(is_tell_requested))
        .count();
    assert_eq!(
        tell_requested_batches,
        1,
        "the TellRequested marker must not be written by an append of its own — \
         a hand-built Sequence must reach the interpreter normalized the same way \
         combine's output does; batches: {:?}",
        describe(&recorded)
    );
}

/// Given a saga returning the hand-built `Sequence(vec![Persist(e),
/// Sequence(vec![]), Persist(tell)])` — an empty nested `Sequence` wedged
/// between the two `Persist` leaves,
/// When the upstream event is handled,
/// Then the domain event and its `TellRequested` marker are still written by
/// a single `store::append` call. An empty `Sequence` carries zero leaves and
/// must not be collapsed into a `SagaEffect::None` boundary that would split
/// an otherwise-adjacent `Persist × Persist` junction.
#[tokio::test]
async fn hand_built_sequence_with_empty_nested_sequence_is_written_in_one_append_batch() {
    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::builder(ps)
        .with_persistence(PersistenceProvider::new(InMemoryStore::default()))
        .build();

    let upstream_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let order_id = AggregateId::new("sequence-empty-nested-order");
    append_order_placed(&upstream_store, &order_id, 1, "SKU-SEQ-EMPTY").await;

    let inventory = system
        .spawn_aggregate::<Inventory>(AggregateId::new("sequence-empty-nested-inventory"))
        .await;

    let batches: Batches = Arc::new(Mutex::new(Vec::new()));
    let saga_store: Arc<dyn EventStore> = Arc::new(RecordingStore::new(Arc::clone(&batches)));
    let saga_id = SagaId::new(PERSIST_SEQUENCED_WITH_EMPTY_NESTED_SEQUENCE_SAGA_ID);
    let handled = Arc::new(Notify::new());

    let inventory_clone = inventory.clone();
    let handled_clone = Arc::clone(&handled);

    let _saga_proxy = SagaProps::<PersistSequencedWithEmptyNestedSequenceSaga>::new(
        saga_id.clone(),
        Arc::clone(&saga_store),
        move |_components| PersistSequencedWithEmptyNestedSequenceSaga {
            inventory: inventory_clone.clone(),
            handled: Arc::clone(&handled_clone),
        },
    )
    .subscribed_to::<OrderPlaced>(Subscription::stream(&upstream_store, &order_id))
    .spawn(system.process_system())
    .await;

    tokio::time::timeout(Duration::from_secs(3), handled.notified())
        .await
        .expect("Saga::handle must run within 3 seconds");

    // domain event + TellRequested + consumed position + the eventual TellAcked.
    let recorded = wait_for_recorded_events(&batches, 4).await;

    let domain_batches: Vec<&Vec<RecordedEvent>> = recorded
        .iter()
        .filter(|batch| batch.iter().any(is_domain_event))
        .collect();
    assert_eq!(
        domain_batches.len(),
        1,
        "the domain event must be appended exactly once; batches: {:?}",
        describe(&recorded)
    );

    let atomic_batch = domain_batches[0];
    assert_eq!(
        atomic_batch.len(),
        3,
        "Sequence(vec![Persist(e), Sequence(vec![]), Persist(tell)]) must append \
         the domain event, the TellRequested marker and the consumed position \
         together — the empty nested Sequence contributes zero leaves and must \
         not become a merge boundary; batches: {:?}",
        describe(&recorded)
    );
    assert!(
        is_domain_event(&atomic_batch[0]),
        "the user event must lead the atomic batch; batches: {:?}",
        describe(&recorded)
    );
    assert!(
        is_tell_requested(&atomic_batch[1]),
        "the TellRequested marker must ride in the same batch as the user event; \
         batches: {:?}",
        describe(&recorded)
    );
    assert!(
        is_consumed_position(&atomic_batch[2]),
        "the position closes the batch, so the marker before it keeps the \
         sequence its tell_id names; batches: {:?}",
        describe(&recorded)
    );

    let tell_requested_batches = recorded
        .iter()
        .filter(|batch| batch.iter().any(is_tell_requested))
        .count();
    assert_eq!(
        tell_requested_batches,
        1,
        "the TellRequested marker must not be written by an append of its own — \
         an empty nested Sequence must not split the atomic batch; \
         batches: {:?}",
        describe(&recorded)
    );
}

/// Given a saga returning the hand-built `Sequence(vec![Persist(e), None,
/// Persist(tell)])` — the Monoid identity wedged between the two `Persist`
/// leaves,
/// When the upstream event is handled,
/// Then the domain event and its `TellRequested` marker are still written by
/// a single `store::append` call. `SagaEffect::None` is a no-op identity; if
/// it survived normalization as a merge boundary, adding an element that
/// interprets to nothing would reopen the at-most-once tell-loss window.
#[tokio::test]
async fn hand_built_sequence_with_none_leaf_is_written_in_one_append_batch() {
    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::builder(ps)
        .with_persistence(PersistenceProvider::new(InMemoryStore::default()))
        .build();

    let upstream_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let order_id = AggregateId::new("sequence-none-leaf-order");
    append_order_placed(&upstream_store, &order_id, 1, "SKU-SEQ-NONE").await;

    let inventory = system
        .spawn_aggregate::<Inventory>(AggregateId::new("sequence-none-leaf-inventory"))
        .await;

    let batches: Batches = Arc::new(Mutex::new(Vec::new()));
    let saga_store: Arc<dyn EventStore> = Arc::new(RecordingStore::new(Arc::clone(&batches)));
    let saga_id = SagaId::new(PERSIST_SEQUENCED_WITH_NONE_LEAF_SAGA_ID);
    let handled = Arc::new(Notify::new());

    let inventory_clone = inventory.clone();
    let handled_clone = Arc::clone(&handled);

    let _saga_proxy = SagaProps::<PersistSequencedWithNoneLeafSaga>::new(
        saga_id.clone(),
        Arc::clone(&saga_store),
        move |_components| PersistSequencedWithNoneLeafSaga {
            inventory: inventory_clone.clone(),
            handled: Arc::clone(&handled_clone),
        },
    )
    .subscribed_to::<OrderPlaced>(Subscription::stream(&upstream_store, &order_id))
    .spawn(system.process_system())
    .await;

    tokio::time::timeout(Duration::from_secs(3), handled.notified())
        .await
        .expect("Saga::handle must run within 3 seconds");

    // domain event + TellRequested + consumed position + the eventual TellAcked.
    let recorded = wait_for_recorded_events(&batches, 4).await;

    let domain_batches: Vec<&Vec<RecordedEvent>> = recorded
        .iter()
        .filter(|batch| batch.iter().any(is_domain_event))
        .collect();
    assert_eq!(
        domain_batches.len(),
        1,
        "the domain event must be appended exactly once; batches: {:?}",
        describe(&recorded)
    );

    let atomic_batch = domain_batches[0];
    assert_eq!(
        atomic_batch.len(),
        3,
        "Sequence(vec![Persist(e), None, Persist(tell)]) must append the domain \
         event, the TellRequested marker and the consumed position together — \
         None is the Monoid identity and must not become a merge boundary; \
         batches: {:?}",
        describe(&recorded)
    );
    assert!(
        is_domain_event(&atomic_batch[0]),
        "the user event must lead the atomic batch; batches: {:?}",
        describe(&recorded)
    );
    assert!(
        is_tell_requested(&atomic_batch[1]),
        "the TellRequested marker must ride in the same batch as the user event; \
         batches: {:?}",
        describe(&recorded)
    );
    assert!(
        is_consumed_position(&atomic_batch[2]),
        "the position closes the batch, so the marker before it keeps the \
         sequence its tell_id names; batches: {:?}",
        describe(&recorded)
    );

    let tell_requested_batches = recorded
        .iter()
        .filter(|batch| batch.iter().any(is_tell_requested))
        .count();
    assert_eq!(
        tell_requested_batches,
        1,
        "the TellRequested marker must not be written by an append of its own — \
         an explicit None must not split the atomic batch; batches: {:?}",
        describe(&recorded)
    );
}

/// Given a persist combined with two tells aimed at different aggregates,
/// When the upstream event is handled,
/// Then all three land in one append batch with the tells in left-to-right
/// order — the `tells` concatenation order of the merge is observable here
/// because `Shape` can only count opaque `TellIntent`s.
#[tokio::test]
async fn combined_tells_keep_left_to_right_order_inside_the_single_batch() {
    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::builder(ps)
        .with_persistence(PersistenceProvider::new(InMemoryStore::default()))
        .build();

    let upstream_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let order_id = AggregateId::new("combine-order-order");
    append_order_placed(&upstream_store, &order_id, 1, "SKU-2").await;

    let first = system
        .spawn_aggregate::<Inventory>(AggregateId::new("combine-order-inv-a"))
        .await;
    let second = system
        .spawn_aggregate::<Inventory>(AggregateId::new("combine-order-inv-b"))
        .await;

    let batches: Batches = Arc::new(Mutex::new(Vec::new()));
    let saga_store: Arc<dyn EventStore> = Arc::new(RecordingStore::new(Arc::clone(&batches)));
    let saga_id = SagaId::new(PERSIST_COMBINE_TWO_TELLS_SAGA_ID);
    let handled = Arc::new(Notify::new());

    let first_clone = first.clone();
    let second_clone = second.clone();
    let handled_clone = Arc::clone(&handled);

    let _saga_proxy = SagaProps::<PersistCombineTwoTellsSaga>::new(
        saga_id.clone(),
        Arc::clone(&saga_store),
        move |_components| PersistCombineTwoTellsSaga {
            first: first_clone.clone(),
            second: second_clone.clone(),
            handled: Arc::clone(&handled_clone),
        },
    )
    .subscribed_to::<OrderPlaced>(Subscription::stream(&upstream_store, &order_id))
    .spawn(system.process_system())
    .await;

    tokio::time::timeout(Duration::from_secs(3), handled.notified())
        .await
        .expect("Saga::handle must run within 3 seconds");

    // domain event + 2 TellRequested + consumed position + the eventual
    // 2 TellAcked.
    let recorded = wait_for_recorded_events(&batches, 6).await;

    let domain_batches: Vec<&Vec<RecordedEvent>> = recorded
        .iter()
        .filter(|batch| batch.iter().any(is_domain_event))
        .collect();
    assert_eq!(
        domain_batches.len(),
        1,
        "the domain event must be appended exactly once; batches: {:?}",
        describe(&recorded)
    );

    let atomic_batch = domain_batches[0];
    assert_eq!(
        atomic_batch.len(),
        4,
        "persist(e).combine(tell(a)).combine(tell(b)) must append the user event, \
         both TellRequested markers and the consumed position together; \
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
        "the position closes the batch, so both tell markers keep the sequences \
         their tell_ids name; batches: {:?}",
        describe(&recorded)
    );
    assert_eq!(
        tell_targets(atomic_batch),
        vec![
            "combine-order-inv-a".to_owned(),
            "combine-order-inv-b".to_owned()
        ],
        "merged tells must keep the left-to-right order of the combine chain; \
         batches: {:?}",
        describe(&recorded)
    );
}

/// Given a saga returning `persist(e).combine(deadline.set(..))`,
/// When the upstream event is handled,
/// Then the domain event and the `scheduled` marker are written by a single
/// `store::append` call — a split append could lose the timer's durable record
/// while the event that justifies it survives.
#[tokio::test]
async fn persist_combined_with_schedule_is_written_in_one_append_batch() {
    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::builder(ps)
        .with_persistence(PersistenceProvider::new(InMemoryStore::default()))
        .build();

    let upstream_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let order_id = AggregateId::new("combine-schedule-order");
    append_order_placed(&upstream_store, &order_id, 1, "SKU-3").await;

    let batches: Batches = Arc::new(Mutex::new(Vec::new()));
    let saga_store: Arc<dyn EventStore> = Arc::new(RecordingStore::new(Arc::clone(&batches)));
    let saga_id = SagaId::new(PERSIST_COMBINE_SCHEDULE_SAGA_ID);
    let handled = Arc::new(Notify::new());

    let handled_clone = Arc::clone(&handled);

    let _saga_proxy = SagaProps::<PersistCombineScheduleSaga>::new(
        saga_id.clone(),
        Arc::clone(&saga_store),
        move |components| PersistCombineScheduleSaga {
            deadline: components.deadline(),
            handled: Arc::clone(&handled_clone),
        },
    )
    .subscribed_to::<OrderPlaced>(Subscription::stream(&upstream_store, &order_id))
    .spawn(system.process_system())
    .await;

    tokio::time::timeout(Duration::from_secs(3), handled.notified())
        .await
        .expect("Saga::handle must run within 3 seconds");

    // domain event + the `scheduled` marker + the consumed position; the timer
    // the host armed is an hour out, so no firing — and so no `fired` marker —
    // reaches the store inside this test.
    let recorded = wait_for_recorded_events(&batches, 3).await;

    let domain_batches: Vec<&Vec<RecordedEvent>> = recorded
        .iter()
        .filter(|batch| batch.iter().any(is_domain_event))
        .collect();
    assert_eq!(
        domain_batches.len(),
        1,
        "the domain event must be appended exactly once; batches: {:?}",
        describe(&recorded)
    );

    let atomic_batch = domain_batches[0];
    assert_eq!(
        atomic_batch.len(),
        3,
        "persist(e).combine(deadline.set(..)) must append the domain event, the \
         scheduled marker and the position of the input they were decided from \
         together; batches: {:?}",
        describe(&recorded)
    );
    assert!(
        is_domain_event(&atomic_batch[0]),
        "the user event must lead the atomic batch; batches: {:?}",
        describe(&recorded)
    );
    assert!(
        is_scheduled_marker(&atomic_batch[1]),
        "the scheduled marker must ride in the same batch as the user event; \
         batches: {:?}",
        describe(&recorded)
    );
    assert!(
        is_consumed_position(&atomic_batch[2]),
        "the position closes the batch, after the rows the effect itself \
         produced; batches: {:?}",
        describe(&recorded)
    );

    let scheduled_batches = recorded
        .iter()
        .filter(|batch| batch.iter().any(is_scheduled_marker))
        .count();
    assert_eq!(
        scheduled_batches,
        1,
        "the scheduled marker must not be written by an append of its own; \
         batches: {:?}",
        describe(&recorded)
    );
}
