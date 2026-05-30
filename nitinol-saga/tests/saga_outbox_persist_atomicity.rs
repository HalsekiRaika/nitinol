//! Spec C-8 atomicity invariant: a `Persist { events, tells, schedules }`
//! branch must append the user events, the per-tell `TellRequested` markers,
//! and the per-schedule `Scheduled` markers in **one** atomic batch on the
//! saga's own event store.
//!
//! Without atomicity, a crash between the user event append and the outbox
//! marker append would leave the system in an inconsistent state — the saga
//! would think it had emitted a tell that no executor will ever pick up on
//! replay.

mod common;
use common::JsonCodec;

use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use futures_util::TryStreamExt;
use serde::{Deserialize, Serialize};
use tokio::sync::Notify;

use nitinol_eventsource::test_helpers::MockAggregateProxy;
use nitinol_eventsource::{
    system::EventSourceSystem, Aggregate, Context, Decider, Effect, Event, SequenceCursor,
};
use nitinol_persistence::store::{EventStore, InMemoryEventStore};
use nitinol_persistence::{AggregateId, AppendingEvent, EventType, LoadQuery, LoadedEvent};
use nitinol_runtime::ProcessSystem;
use nitinol_saga::{Saga, SagaContext, SagaEffect, SagaId, SagaProps};

const OUTBOX_PREFIX: &str = "nitinol.saga.outbox.";

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
struct OrderPlaced {
    sku: String,
}

impl Event for OrderPlaced {
    const EVENT_TYPE: EventType = EventType::from_str("atomic.OrderPlaced");
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ReservationRequested {
    sku: String,
}

impl Event for ReservationRequested {
    const EVENT_TYPE: EventType = EventType::from_str("atomic.ReservationRequested");
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
    const EVENT_TYPE: EventType = EventType::from_str("atomic.InventoryReserved");
}

#[derive(Clone)]
struct Reserve {
    sku: String,
}

#[async_trait]
impl Decider<Reserve> for Inventory {
    type Rejection = std::convert::Infallible;
    async fn decide(
        &self,
        cmd: Reserve,
        _ctx: &mut Context,
    ) -> Result<Effect<InventoryReserved>, Self::Rejection> {
        Ok(Effect::persist(InventoryReserved { sku: cmd.sku }))
    }
}

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
    type SubscribedEvent = OrderPlaced;
    type Event = ReservationRequested;
    type State = ();
    type Error = std::convert::Infallible;

    fn apply(&mut self, _event: Self::Event) {}

    async fn handle(
        &mut self,
        event: Self::SubscribedEvent,
        _ctx: &mut SagaContext,
    ) -> Result<SagaEffect<Self::Event>, Self::Error> {
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
        .with_tells(vec![intent_a, intent_b]);

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
    let system = EventSourceSystem::new(ps).with_codec::<JsonCodec>().build();

    let upstream_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let order_id = AggregateId::new("atomic-order");
    append_order_placed(&upstream_store, &order_id, 1, "SKU-A").await;

    let saga_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let saga_id = SagaId::new("atomic-saga-1");
    let handled = Arc::new(Notify::new());

    let mock_a = MockAggregateProxy::<Inventory>::new();
    let mock_b = MockAggregateProxy::<Inventory>::new();
    let mock_a_clone = mock_a.clone();
    let mock_b_clone = mock_b.clone();
    let handled_clone = Arc::clone(&handled);

    let routed = saga_id.clone();
    let route_fn = move |_event: &OrderPlaced| -> Option<SagaId> { Some(routed.clone()) };

    let _saga_proxy =
        SagaProps::<MultiTellSaga>::new(saga_id.clone(), Arc::clone(&saga_store), move || {
            MultiTellSaga {
                inventory: mock_a_clone.clone(),
                other_inventory: mock_b_clone.clone(),
                handled: Arc::clone(&handled_clone),
            }
        })
        .with_codec(system.codec::<ReservationRequested>())
        .with_subscription(
            Arc::clone(&upstream_store),
            system.codec::<OrderPlaced>(),
            SequenceCursor::Stream {
                key: order_id.as_str().to_owned(),
                after: 0,
            },
            route_fn,
        )
        .spawn(system.process_system())
        .await;

    tokio::time::timeout(Duration::from_secs(3), handled.notified())
        .await
        .expect("Saga::handle must run within 3 seconds");

    // Expect at least: 1 user event + 2 TellRequested + (eventually) 2 TellAcked.
    // We wait for all 5 to land so the assertions below are stable.
    let events = wait_for_event_count(&saga_store, &saga_id, 5).await;

    let user_events: Vec<&LoadedEvent> = events
        .iter()
        .filter(|e| e.event_type == ReservationRequested::EVENT_TYPE)
        .collect();
    let requested: Vec<&LoadedEvent> = events
        .iter()
        .filter(|e| {
            let s = e.event_type.as_str();
            s.starts_with(OUTBOX_PREFIX) && s.ends_with("tell_requested")
        })
        .collect();
    let acked: Vec<&LoadedEvent> = events
        .iter()
        .filter(|e| {
            let s = e.event_type.as_str();
            s.starts_with(OUTBOX_PREFIX) && s.ends_with("tell_acked")
        })
        .collect();
    let failed: Vec<&LoadedEvent> = events
        .iter()
        .filter(|e| {
            let s = e.event_type.as_str();
            s.starts_with(OUTBOX_PREFIX) && s.ends_with("tell_failed")
        })
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

    // The atomic batch is the *initial* burst: user event + 2 TellRequested
    // appended at consecutive sequences with the same occurred_at (because the
    // saga interpreter builds them all from one `jiff::Timestamp::now()` call).
    let mut atomic_batch: Vec<&LoadedEvent> = events
        .iter()
        .filter(|e| {
            e.event_type == ReservationRequested::EVENT_TYPE
                || {
                    let s = e.event_type.as_str();
                    s.starts_with(OUTBOX_PREFIX) && s.ends_with("tell_requested")
                }
        })
        .collect();
    atomic_batch.sort_by_key(|e| e.sequence);

    assert_eq!(
        atomic_batch.len(),
        3,
        "atomic Persist batch must be exactly 3 events (1 user + 2 TellRequested)"
    );
    let seqs: Vec<u64> = atomic_batch.iter().map(|e| e.sequence).collect();
    assert_eq!(
        seqs,
        vec![1, 2, 3],
        "the atomic batch must occupy consecutive sequences starting from 1 — \
         a gap would mean the appends were not in the same store::append call"
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
    struct UserOnlySaga {
        notify: Arc<Notify>,
        captured: Arc<Mutex<Vec<String>>>,
    }

    #[async_trait]
    impl Saga for UserOnlySaga {
        type SubscribedEvent = OrderPlaced;
        type Event = ReservationRequested;
        type State = ();
        type Error = std::convert::Infallible;

        fn apply(&mut self, _event: Self::Event) {}

        async fn handle(
            &mut self,
            event: Self::SubscribedEvent,
            _ctx: &mut SagaContext,
        ) -> Result<SagaEffect<Self::Event>, Self::Error> {
            self.captured.lock().unwrap().push(event.sku.clone());
            let effect = SagaEffect::persist(ReservationRequested { sku: event.sku });
            self.notify.notify_one();
            Ok(effect)
        }
    }

    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::new(ps).with_codec::<JsonCodec>().build();

    let upstream_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let order_id = AggregateId::new("user-only-order");
    append_order_placed(&upstream_store, &order_id, 1, "USR-ONLY-1").await;

    let saga_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let saga_id = SagaId::new("user-only-saga-1");
    let notify = Arc::new(Notify::new());
    let captured: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));

    let notify_clone = Arc::clone(&notify);
    let captured_clone = Arc::clone(&captured);

    let routed = saga_id.clone();
    let route_fn = move |_event: &OrderPlaced| -> Option<SagaId> { Some(routed.clone()) };

    let _saga_proxy =
        SagaProps::<UserOnlySaga>::new(saga_id.clone(), Arc::clone(&saga_store), move || {
            UserOnlySaga {
                notify: Arc::clone(&notify_clone),
                captured: Arc::clone(&captured_clone),
            }
        })
        .with_codec(system.codec::<ReservationRequested>())
        .with_subscription(
            Arc::clone(&upstream_store),
            system.codec::<OrderPlaced>(),
            SequenceCursor::Stream {
                key: order_id.as_str().to_owned(),
                after: 0,
            },
            route_fn,
        )
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
        .filter(|e| e.event_type.as_str().starts_with(OUTBOX_PREFIX))
        .collect();

    assert_eq!(
        events.len(),
        1,
        "saga must persist exactly one event for a tell-less Persist branch"
    );
    assert!(
        outbox_events.is_empty(),
        "no outbox markers must be emitted when the Persist branch has no tells / no schedules; \
         got: {:?}",
        outbox_events.iter().map(|e| e.event_type.as_str()).collect::<Vec<_>>()
    );
    assert_eq!(events[0].event_type, ReservationRequested::EVENT_TYPE);
}
