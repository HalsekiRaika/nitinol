//! Atomicity invariant: a `Persist { events, tells, schedules }`
//! branch must append the user events, the per-tell `TellRequested` markers,
//! and the per-schedule `Scheduled` markers in **one** atomic batch on the
//! saga's own event store.
//!
//! The sagas here return no hard boundary — no `End`, no `CancelSchedule` —
//! so that one `Persist` is also the append closing their input out, and the
//! consumed position rides it.  Which append carries a position is the
//! interpreter's rule rather than part of this invariant: a `Persist` that is
//! not the one closing the input out writes no position at all.
//!
//! Without atomicity, a crash between the user event append and the outbox
//! marker append would leave the system in an inconsistent state — the saga
//! would think it had emitted a tell that no executor will ever pick up on
//! replay.

#[path = "common/helpers.rs"]
mod common;
use common::{outbox_kind_of, JsonCodec, OutboxKind, CONSUMED_POSITION_MARKER};

use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use futures_util::TryStreamExt;
use serde::{Deserialize, Serialize};
use tokio::sync::Notify;

use nitinol_eventsource::test_helpers::MockAggregateProxy;
use nitinol_eventsource::{system::EventSourceSystem, Aggregate, Decider, Decision, Event};
use nitinol_persistence::provider::Persistence;
use nitinol_persistence::store::{EventStore, InMemoryEventStore};
use nitinol_persistence::{
    AggregateId, AppendingEvent, EventType, Family, LoadQuery, LoadedEvent, TypeName,
};
use nitinol_runtime::ProcessSystem;
use nitinol_saga::{Saga, SagaContext, SagaEffect, SagaId, SagaProps, SagaSubscribe, Subscription};

/// The whole alphabet both sagas in this file react to: one observed source, no
/// deadline.
#[derive(SagaSubscribe)]
enum Observed {
    Order(OrderPlaced),
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
struct OrderPlaced {
    sku: String,
}

impl Event for OrderPlaced {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("atomic"), TypeName::new("OrderPlaced"));
    type Codec = JsonCodec;
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ReservationRequested {
    sku: String,
}

impl Event for ReservationRequested {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("atomic"), TypeName::new("ReservationRequested"));
    type Codec = JsonCodec;
}

#[derive(Default)]
struct Inventory;

impl Aggregate for Inventory {
    type Event = InventoryReserved;
    fn apply(&mut self, _event: InventoryReserved) {}
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct InventoryReserved {
    sku: String,
}

impl Event for InventoryReserved {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("atomic"), TypeName::new("InventoryReserved"));
    type Codec = JsonCodec;
}

#[derive(Clone)]
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

/// Correlation rule of [`MultiTellSaga`]: one order process owns every
/// `OrderPlaced` published in this file's atomic-batch test.
const MULTI_TELL_SAGA_ID: &str = "atomic-saga-1";

/// A saga that emits `Persist { events, tells, schedules }` where:
/// - events: one ReservationRequested
/// - tells: two Reserve commands (different SKUs)
/// - schedules: empty (covered separately by saga_schedule_variant_appends_marker.rs)
///
/// The shape lets us verify the atomic batch contains 1 user event + 2
/// TellRequested markers, all appended with consecutive sequences in the
/// same store::append call.
struct MultiTellSaga {
    inventory: MockAggregateProxy<Inventory>,
    handled: Arc<Notify>,
    other_inventory: MockAggregateProxy<Inventory>,
}

#[async_trait]
impl Saga for MultiTellSaga {
    type Subscribe = Observed;
    type Event = ReservationRequested;
    type Error = std::convert::Infallible;

    fn correlate(_input: &Self::Subscribe) -> Option<SagaId> {
        Some(SagaId::new(MULTI_TELL_SAGA_ID))
    }

    async fn handle(
        &mut self,
        input: Self::Subscribe,
        _ctx: &mut SagaContext,
    ) -> Result<SagaEffect<Self::Event>, Self::Error> {
        let Observed::Order(event) = input;
        let intent_a = nitinol_saga::TellIntent::new(
            self.inventory.clone(),
            Reserve {
                sku: format!("{}-A", event.sku),
            },
        );
        let intent_b = nitinol_saga::TellIntent::new(
            self.other_inventory.clone(),
            Reserve {
                sku: format!("{}-B", event.sku),
            },
        );
        let effect = SagaEffect::persist(ReservationRequested {
            sku: event.sku.clone(),
        })
        .combine(SagaEffect::tell_intent(intent_a))
        .combine(SagaEffect::tell_intent(intent_b));

        self.handled.notify_one();
        Ok(effect)
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

async fn load_saga_events(store: &Arc<dyn EventStore>, saga_id: &SagaId) -> Vec<LoadedEvent> {
    store
        .load(LoadQuery::by_stream(saga_id))
        .await
        .expect("load saga stream must succeed")
        .try_collect()
        .await
        .expect("collect saga events must succeed")
}

async fn wait_for_event_count(
    store: &Arc<dyn EventStore>,
    saga_id: &SagaId,
    expected_min: usize,
) -> Vec<LoadedEvent> {
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    loop {
        let events = load_saga_events(store, saga_id).await;
        if events.len() >= expected_min {
            return events;
        }
        if std::time::Instant::now() >= deadline {
            panic!(
                "timed out waiting for {expected_min} events in saga stream (had {})",
                events.len()
            );
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

#[tokio::test]
async fn persist_with_two_tells_appends_user_event_and_two_outbox_markers_atomically() {
    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::builder(ps)
        .with_persistence(Persistence::in_memory())
        .build();

    let upstream_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let order_id = AggregateId::new("atomic-order");
    append_order_placed(&upstream_store, &order_id, 1, "SKU-A").await;

    let saga_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let saga_id = SagaId::new(MULTI_TELL_SAGA_ID);
    let handled = Arc::new(Notify::new());

    let mock_a = MockAggregateProxy::<Inventory>::new();
    let mock_b = MockAggregateProxy::<Inventory>::new();
    let mock_a_clone = mock_a.clone();
    let mock_b_clone = mock_b.clone();
    let handled_clone = Arc::clone(&handled);

    let _saga_proxy = SagaProps::<MultiTellSaga>::new(
        saga_id.clone(),
        Arc::clone(&saga_store),
        move |_components| MultiTellSaga {
            inventory: mock_a_clone.clone(),
            other_inventory: mock_b_clone.clone(),
            handled: Arc::clone(&handled_clone),
        },
    )
    .subscribed_to::<OrderPlaced>(Subscription::stream(&upstream_store, &order_id))
    .spawn(system.process_system())
    .await;

    tokio::time::timeout(Duration::from_secs(3), handled.notified())
        .await
        .expect("Saga::handle must run within 3 seconds");

    // Expect at least: 1 user event + 2 TellRequested + 1 consumed position +
    // (eventually) 2 TellAcked.  We wait for all 6 to land so the assertions
    // below are stable.
    let events = wait_for_event_count(&saga_store, &saga_id, 6).await;

    let user_events: Vec<&LoadedEvent> = events
        .iter()
        .filter(|e| e.event_type == ReservationRequested::EVENT_TYPE)
        .collect();
    let requested: Vec<&LoadedEvent> = events
        .iter()
        .filter(|e| matches!(outbox_kind_of(e), Some(OutboxKind::TellRequested(_))))
        .collect();
    let acked: Vec<&LoadedEvent> = events
        .iter()
        .filter(|e| matches!(outbox_kind_of(e), Some(OutboxKind::TellAcked(_))))
        .collect();
    let failed: Vec<&LoadedEvent> = events
        .iter()
        .filter(|e| matches!(outbox_kind_of(e), Some(OutboxKind::TellFailed(_))))
        .collect();

    assert_eq!(user_events.len(), 1, "must persist exactly one user event");
    assert_eq!(
        requested.len(),
        2,
        "Persist with two tells must append exactly two TellRequested markers"
    );
    assert_eq!(
        acked.len(),
        2,
        "both tells must eventually be Acked once the dispatch succeeds"
    );
    assert_eq!(
        failed.len(),
        0,
        "no TellFailed must be emitted when every dispatch succeeds first try"
    );

    // The atomic batch is the *initial* burst: user event + 2 TellRequested +
    // the consumed position of the upstream input, appended at consecutive
    // sequences with the same occurred_at (because the saga interpreter builds
    // them all from one `jiff::Timestamp::now()` call).  The position closes
    // the batch, which is what leaves each `tell_id` equal to the sequence of
    // its own marker.
    let mut atomic_batch: Vec<&LoadedEvent> = events
        .iter()
        .filter(|e| {
            e.event_type == ReservationRequested::EVENT_TYPE
                || e.event_type == CONSUMED_POSITION_MARKER
                || matches!(outbox_kind_of(e), Some(OutboxKind::TellRequested(_)))
        })
        .collect();
    atomic_batch.sort_by_key(|e| e.sequence);

    assert_eq!(
        atomic_batch.len(),
        4,
        "atomic Persist batch must be exactly 4 events (1 user + 2 TellRequested \
         + 1 consumed position)"
    );
    let seqs: Vec<u64> = atomic_batch.iter().map(|e| e.sequence).collect();
    assert_eq!(
        seqs,
        vec![1, 2, 3, 4],
        "the atomic batch must occupy consecutive sequences starting from 1 — \
         a gap would mean the appends were not in the same store::append call"
    );
    assert_eq!(
        atomic_batch[3].event_type, CONSUMED_POSITION_MARKER,
        "the position is the last row of the batch"
    );
    let tell_ids: Vec<(u64, u64)> = atomic_batch
        .iter()
        .filter_map(|e| match outbox_kind_of(e) {
            Some(OutboxKind::TellRequested(m)) => Some((e.sequence, m.tell_id)),
            _ => None,
        })
        .collect();
    assert_eq!(
        tell_ids,
        vec![(2, 2), (3, 3)],
        "a tell is identified by the stream sequence of the marker that \
         requested it; a row inserted before them would repoint every pending \
         tell"
    );

    // Sanity: both mock targets received exactly one Reserve each (the
    // single-attempt success path must not double-dispatch).
    let captured_a = mock_a.drain_captured::<Reserve>();
    let captured_b = mock_b.drain_captured::<Reserve>();
    assert_eq!(captured_a.len(), 1, "first tell target must receive 1 cmd");
    assert_eq!(captured_b.len(), 1, "second tell target must receive 1 cmd");
    assert_eq!(captured_a[0].sku, "SKU-A-A");
    assert_eq!(captured_b[0].sku, "SKU-A-B");
}

#[tokio::test]
async fn persist_user_events_alone_does_not_emit_outbox_markers() {
    // Baseline check: a Persist branch with no tells and no schedules must
    // produce *only* the user events in the saga stream — outbox markers must
    // only appear when there is something to mark.

    /// Correlation rule of `UserOnlySaga`: one process owns the single
    /// `OrderPlaced` this test publishes.
    const USER_ONLY_SAGA_ID: &str = "user-only-saga-1";

    struct UserOnlySaga {
        notify: Arc<Notify>,
        captured: Arc<Mutex<Vec<String>>>,
    }

    #[async_trait]
    impl Saga for UserOnlySaga {
        type Subscribe = Observed;
        type Event = ReservationRequested;
        type Error = std::convert::Infallible;

        fn correlate(_input: &Self::Subscribe) -> Option<SagaId> {
            Some(SagaId::new(USER_ONLY_SAGA_ID))
        }

        async fn handle(
            &mut self,
            input: Self::Subscribe,
            _ctx: &mut SagaContext,
        ) -> Result<SagaEffect<Self::Event>, Self::Error> {
            let Observed::Order(event) = input;
            self.captured
                .lock()
                .expect(
                    "captured mutex is never poisoned: no holder panics while the guard is alive",
                )
                .push(event.sku.clone());
            let effect = SagaEffect::persist(ReservationRequested { sku: event.sku });
            self.notify.notify_one();
            Ok(effect)
        }
    }

    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::builder(ps)
        .with_persistence(Persistence::in_memory())
        .build();

    let upstream_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let order_id = AggregateId::new("user-only-order");
    append_order_placed(&upstream_store, &order_id, 1, "USR-ONLY-1").await;

    let saga_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let saga_id = SagaId::new(USER_ONLY_SAGA_ID);
    let notify = Arc::new(Notify::new());
    let captured: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));

    let notify_clone = Arc::clone(&notify);
    let captured_clone = Arc::clone(&captured);

    let _saga_proxy = SagaProps::<UserOnlySaga>::new(
        saga_id.clone(),
        Arc::clone(&saga_store),
        move |_components| UserOnlySaga {
            notify: Arc::clone(&notify_clone),
            captured: Arc::clone(&captured_clone),
        },
    )
    .subscribed_to::<OrderPlaced>(Subscription::stream(&upstream_store, &order_id))
    .spawn(system.process_system())
    .await;

    tokio::time::timeout(Duration::from_secs(3), notify.notified())
        .await
        .expect("Saga::handle must run");

    // Let any spurious outbox writes drain
    tokio::time::sleep(Duration::from_millis(100)).await;

    let events = load_saga_events(&saga_store, &saga_id).await;
    let outbox_events: Vec<&LoadedEvent> = events
        .iter()
        .filter(|e| outbox_kind_of(e).is_some())
        .collect();

    let user_events: Vec<&LoadedEvent> = events
        .iter()
        .filter(|e| e.event_type == ReservationRequested::EVENT_TYPE)
        .collect();
    assert_eq!(
        user_events.len(),
        1,
        "saga must persist exactly one user event for a tell-less Persist branch"
    );
    assert_eq!(
        events.len(),
        2,
        "the only other row is the position of the input it was decided from; \
         got: {:?}",
        events
            .iter()
            .map(|e| e.event_type.to_string())
            .collect::<Vec<_>>()
    );
    assert_eq!(
        events[1].event_type, CONSUMED_POSITION_MARKER,
        "the position is a sibling declaration of the outbox, which is why the \
         outbox assertion below still holds"
    );
    assert!(
        outbox_events.is_empty(),
        "no outbox markers must be emitted when the Persist branch has no tells / no schedules; \
         got: {:?}",
        outbox_events
            .iter()
            .map(|e| e.event_type.to_string())
            .collect::<Vec<_>>()
    );
    assert_eq!(events[0].event_type, ReservationRequested::EVENT_TYPE);
}
