//! What an *empty* `Persist` means to the interpreter.
//!
//! `SagaEffect::persist_all(vec![])` keeps a `Persist` variant structurally —
//! it stays a valid `combine` receiver — but a `Persist` whose `events`,
//! `tells` **and** `schedules` are all empty contributes no batch of its own,
//! exactly like `SagaEffect::None`. Neither is a true no-op, though: the
//! input it was decided from can still have a position pending, and the
//! interpreter writes a consumed-position-only append for it so the input is
//! not redelivered to `Saga::handle`. These tests pin that observable
//! behaviour so the rustdoc on `persist_all` can state it truthfully.
//!
//! The second test pins the other side of the condition, which is the part
//! easiest to get wrong: emptiness of `events` alone is **not** enough. A
//! `Persist` carrying no user events but one tell is what `SagaEffect::tell`
//! itself builds, and collapsing it to a no-op would silently discard every
//! tell a saga ever emits.
//!
//! The first test makes the saga handle a second, event-producing upstream
//! message after the empty one so the batch each input produced can be told
//! apart. Upstream messages are interpreted strictly sequentially (the poller
//! `ask()`s and `recv` awaits the interpreter), so once the second message's
//! batch is recorded the first message's interpretation has already finished
//! and the total batch count is a complete observation.

#[path = "common/helpers.rs"]
mod common;
use common::{decode_outbox_kind, JsonCodec, OutboxKind, CONSUMED_POSITION_MARKER, OUTBOX_MARKER};

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use serde::{Deserialize, Serialize};

use nitinol_eventsource::{
    system::EventSourceSystem, Aggregate, AggregateProxy, Decider, Decision, Event,
};
use nitinol_persistence::error::{AppendError, LoadError};
use nitinol_persistence::store::{EventStore, EventStream, InMemoryEventStore};
use nitinol_persistence::{
    AggregateId, AppendOutcome, AppendingEvent, EventType, Family, LoadQuery, TypeName,
};
use nitinol_runtime::ProcessSystem;
use nitinol_saga::{Saga, SagaContext, SagaEffect, SagaId, SagaProps, SagaSubscribe, Subscription};

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

#[derive(Clone, Debug, Serialize, Deserialize)]
struct InventoryReserved {
    sku: String,
}

impl Event for InventoryReserved {
    const EVENT_TYPE: EventType = EventType::new(
        Family::new("empty_persist"),
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

/// `EventStore` decorator recording one entry per successful `append` call.
///
/// Kept local rather than shared with `saga_combine_persist_single_append.rs`:
/// that file is the standing evidence for the #69 atomic-batch contract and is
/// deliberately left untouched by this change.
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

/// The whole alphabet both sagas in this file react to: one observed source,
/// no deadline.
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

/// Returns a bare tell: a `Persist` with no user events but one tell.
struct TellOnlySaga {
    inventory: AggregateProxy<Inventory>,
    handled: Arc<AtomicUsize>,
}

const TELL_ONLY_SAGA_ID: &str = "tell-only-saga";

#[async_trait]
impl Saga for TellOnlySaga {
    type Subscribe = Observed;
    type Event = ReservationRequested;
    type Error = std::convert::Infallible;

    fn correlate(_input: &Self::Subscribe) -> Option<SagaId> {
        Some(SagaId::new(TELL_ONLY_SAGA_ID))
    }

    async fn handle(
        &mut self,
        input: Self::Subscribe,
        _ctx: &mut SagaContext,
    ) -> Result<SagaEffect<Self::Event>, Self::Error> {
        let Observed::Order(event) = input;
        self.handled.fetch_add(1, Ordering::SeqCst);
        Ok(SagaEffect::tell(
            self.inventory.clone(),
            Reserve { sku: event.sku },
        ))
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

/// Given a saga that returns `persist_all(vec![])` — a `Persist` whose events,
/// tells and schedules are all empty — followed by one that returns a real
/// event,
/// When both upstream events are handled,
/// Then the store receives one `append` call per input: the empty `Persist`
/// carries nothing of its own, exactly like `SagaEffect::None`, but its
/// append still carries the consumed position of the input it was decided
/// from; the real event's append carries the domain event and its own
/// position.
#[tokio::test]
async fn all_empty_persist_writes_only_the_consumed_position() {
    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::builder(ps).build();

    let upstream_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let order_id = AggregateId::new("empty-persist-order");
    append_order_placed(&upstream_store, &order_id, 1, EMPTY_SKU).await;
    append_order_placed(&upstream_store, &order_id, 2, REAL_SKU).await;

    let batches: Batches = Arc::new(Mutex::new(Vec::new()));
    let saga_store: Arc<dyn EventStore> = Arc::new(RecordingStore::new(Arc::clone(&batches)));
    let handled = Arc::new(AtomicUsize::new(0));

    let handled_clone = Arc::clone(&handled);
    let _saga_proxy = SagaProps::<EmptyThenRealSaga>::new(
        SagaId::new(EMPTY_THEN_REAL_SAGA_ID),
        Arc::clone(&saga_store),
        move |_components| EmptyThenRealSaga {
            handled: Arc::clone(&handled_clone),
        },
    )
    .subscribed_to::<OrderPlaced>(Subscription::stream(&upstream_store, &order_id))
    .spawn(system.process_system())
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
        CONSUMED_POSITION_MARKER,
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
        CONSUMED_POSITION_MARKER,
        "the position of the input it was decided from closes the batch; \
         batches: {:?}",
        describe(&recorded)
    );
}

/// Given a saga returning a bare tell — a `Persist` with no user events but one
/// tell,
/// When the upstream event is handled,
/// Then the `TellRequested` marker is appended: emptiness of `events` alone
/// must never make a `Persist` a no-op, or every tell a saga emits would be
/// silently dropped.
#[tokio::test]
async fn persist_with_no_events_but_a_tell_is_not_a_noop() {
    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::builder(ps).build();

    let upstream_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let order_id = AggregateId::new("tell-only-order");
    append_order_placed(&upstream_store, &order_id, 1, "SKU-TELL").await;

    let inventory_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let inventory = system
        .spawn_aggregate::<Inventory>(AggregateId::new("tell-only-inventory"), inventory_store)
        .await;

    let batches: Batches = Arc::new(Mutex::new(Vec::new()));
    let saga_store: Arc<dyn EventStore> = Arc::new(RecordingStore::new(Arc::clone(&batches)));
    let handled = Arc::new(AtomicUsize::new(0));

    let inventory_clone = inventory.clone();
    let handled_clone = Arc::clone(&handled);
    let _saga_proxy = SagaProps::<TellOnlySaga>::new(
        SagaId::new(TELL_ONLY_SAGA_ID),
        Arc::clone(&saga_store),
        move |_components| TellOnlySaga {
            inventory: inventory_clone.clone(),
            handled: Arc::clone(&handled_clone),
        },
    )
    .subscribed_to::<OrderPlaced>(Subscription::stream(&upstream_store, &order_id))
    .spawn(system.process_system())
    .await;

    // The TellRequested marker; the eventual TellAcked follows in its own batch.
    let recorded = wait_for_recorded_events(&batches, 1).await;

    assert!(
        recorded
            .iter()
            .any(|batch| batch.iter().any(is_tell_requested)),
        "a Persist carrying a tell but no user events must still be interpreted — \
         the TellRequested marker must reach the store; batches: {:?}",
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
