//! End-to-end test: Aggregate + Saga share one `Arc<dyn EventStore>` (Issue #40).
//!
//! Validates the full data flow exposed by the refactor:
//!
//! - An `Aggregate` persists events via `store.append(agg_id.borrow(), …)`.
//! - A `Saga` subscribes to a publish stream of those events.
//! - The same `Arc<dyn EventStore>` instance is used by the saga for its own
//!   event stream, keyed by `saga_id.borrow()`.
//! - Replaying the saga from the store on a fresh spawn restores its
//!   sequence — confirming that `SagaProcess::on_start` calls
//!   `store.load(LoadQuery::by_stream(saga_id.borrow()))` directly.
//!
//! This exercises three modules end-to-end (`nitinol-persistence`,
//! `nitinol-eventsource`, `nitinol-saga`) using only the new direct-store
//! API surface.

mod common;
use common::JsonCodec;

use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use futures_util::TryStreamExt;
use serde::{Deserialize, Serialize};
use tokio::sync::Notify;

use nitinol_eventsource::{
    system::EventSourceSystem, Aggregate, Context, Decider, Effect, Event, EventEnvelope,
};
use nitinol_persistence::store::{EventStore, InMemoryEventStore};
use nitinol_persistence::{AggregateId, EventType, LoadQuery};
use nitinol_runtime::ident::ProcessName;
use nitinol_runtime::ProcessSystem;
use nitinol_saga::{Saga, SagaContext, SagaEffect, SagaId, SagaProps};

// ---------------------------------------------------------------------------
// Aggregate fixture — emits OrderPlaced and publishes it to a stream
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
struct OrderPlaced {
    sku: String,
}

impl Event for OrderPlaced {
    const EVENT_TYPE: EventType = EventType::from_str("e2e.direct.OrderPlaced");
}

#[derive(Default)]
struct Order;

impl Aggregate for Order {
    type Event = OrderPlaced;
    fn apply(&mut self, _event: OrderPlaced) {}
}

struct PlaceOrder {
    sku: String,
    stream: nitinol_runtime::process::ProcessProxy<
        nitinol_runtime::process::Stream<EventEnvelope<OrderPlaced>>,
    >,
}

#[async_trait]
impl Decider<PlaceOrder> for Order {
    type Rejection = std::convert::Infallible;

    async fn decide(
        &self,
        cmd: PlaceOrder,
        ctx: &mut Context,
    ) -> Result<Effect<OrderPlaced>, Self::Rejection> {
        let envelope = EventEnvelope {
            aggregate_id: ctx.aggregate_id().clone(),
            sequence: ctx.sequence() + 1,
            global_sequence: 0,
            event: OrderPlaced {
                sku: cmd.sku.clone(),
            },
        };
        Ok(Effect::persist(OrderPlaced { sku: cmd.sku })
            .combine(Effect::publish(cmd.stream, envelope)))
    }
}

// ---------------------------------------------------------------------------
// Saga fixture — records each routed OrderPlaced as a ReservationRequested
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ReservationRequested {
    sku: String,
}

impl Event for ReservationRequested {
    const EVENT_TYPE: EventType = EventType::from_str("e2e.direct.ReservationRequested");
}

struct RecordingSaga {
    captured: Arc<Mutex<Vec<SagaId>>>,
    done: Arc<Notify>,
}

#[async_trait]
impl Saga for RecordingSaga {
    type SubscribedEvent = OrderPlaced;
    type Event = ReservationRequested;
    type State = ();
    type Error = std::convert::Infallible;

    fn apply(&mut self, _event: Self::Event) {}

    async fn handle(
        &mut self,
        event: Self::SubscribedEvent,
        ctx: &mut SagaContext,
    ) -> Result<SagaEffect<Self::Event>, Self::Error> {
        self.captured.lock().unwrap().push(ctx.saga_id().clone());
        let notify = Arc::clone(&self.done);
        let effect = SagaEffect::persist(ReservationRequested { sku: event.sku });
        notify.notify_one();
        Ok(effect)
    }
}

// ---------------------------------------------------------------------------
// End-to-end: aggregate publish → saga handle → saga persist via direct store
// ---------------------------------------------------------------------------

/// One `Arc<dyn EventStore>` is shared by the aggregate and the saga.
/// After the aggregate publishes an OrderPlaced through the stream, the
/// saga handles it, persists its own ReservationRequested event, and the
/// persisted event is readable from the same physical store via the
/// `SagaId`'s `Borrow<str>` key.
#[tokio::test]
async fn aggregate_and_saga_share_one_arc_dyn_event_store() {
    // Given
    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::new(ps).with_codec::<JsonCodec>().build();

    // One physical store serves both the aggregate and the saga
    let store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());

    let order_proxy = system
        .spawn_aggregate::<Order>(AggregateId::new("direct-order"), Arc::clone(&store))
        .await;

    let stream = system
        .process_system()
        .spawn_stream::<EventEnvelope<OrderPlaced>>(ProcessName::new("direct-store-stream"))
        .await
        .expect("spawn_stream must succeed");

    let saga_id = SagaId::new("direct-store-saga-1");
    let captured: Arc<Mutex<Vec<SagaId>>> = Arc::new(Mutex::new(Vec::new()));
    let done = Arc::new(Notify::new());

    let captured_for_producer = Arc::clone(&captured);
    let done_for_producer = Arc::clone(&done);

    let routed = saga_id.clone();
    let route_fn = move |_event: &OrderPlaced| -> Option<SagaId> { Some(routed.clone()) };

    let _saga_proxy = SagaProps::<RecordingSaga>::new(
        saga_id.clone(),
        Arc::clone(&store),
        move || RecordingSaga {
            captured: Arc::clone(&captured_for_producer),
            done: Arc::clone(&done_for_producer),
        },
    )
    .with_codec(system.codec::<ReservationRequested>())
    .with_subscription(stream.clone(), route_fn)
    .spawn(system.process_system())
    .await;

    // When: the aggregate produces an event that the saga is subscribed to
    order_proxy
        .ask(PlaceOrder {
            sku: "SKU-direct-1".into(),
            stream: stream.clone(),
        })
        .await
        .expect("ask(PlaceOrder) must succeed");

    tokio::time::timeout(Duration::from_millis(500), done.notified())
        .await
        .expect("saga must persist within 500ms");

    // Give the runtime a tick to finish the append after notify_one
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Then: the saga's own stream contains exactly one ReservationRequested,
    //       readable from the same store via the SagaId's Borrow<str> key.
    let saga_events: Vec<_> = store
        .load(LoadQuery::by_stream(&saga_id))
        .await
        .expect("load saga stream must succeed")
        .try_collect()
        .await
        .expect("collect saga events must succeed");

    assert_eq!(
        saga_events.len(),
        1,
        "saga must persist exactly one ReservationRequested event"
    );
    assert_eq!(saga_events[0].sequence, 1);
    assert_eq!(saga_events[0].event_type, ReservationRequested::EVENT_TYPE);
    assert_eq!(
        saga_events[0].stream_key,
        saga_id.as_str(),
        "saga event must be keyed by saga_id (Borrow<str>) — not by an aggregate id"
    );

    // And the order stream coexists in the same store under its AggregateId key
    let order_events: Vec<_> = store
        .load(LoadQuery::by_stream("direct-order"))
        .await
        .expect("load order stream must succeed")
        .try_collect()
        .await
        .expect("collect order events must succeed");
    assert_eq!(
        order_events.len(),
        1,
        "aggregate stream must coexist with saga stream in the same store"
    );

    // Captured saga ids must match the spawned saga id
    let captured = captured.lock().unwrap();
    assert_eq!(captured.len(), 1);
    assert_eq!(captured[0].as_str(), "direct-store-saga-1");
}

// ---------------------------------------------------------------------------
// Replay: a fresh SagaProcess restores sequence from the shared store
// ---------------------------------------------------------------------------

/// After persisting one event, dropping the saga, and re-spawning with the
/// same SagaId and same Arc<dyn EventStore>, the saga's `on_start` replays
/// its own stream via direct `store.load` and continues from sequence 2 on
/// the next persist.
#[tokio::test]
async fn saga_replays_its_own_stream_via_direct_store_on_respawn() {
    // Given: a shared store and an existing saga event at sequence 1
    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::new(ps).with_codec::<JsonCodec>().build();
    let store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());

    let order_proxy = system
        .spawn_aggregate::<Order>(AggregateId::new("replay-order"), Arc::clone(&store))
        .await;

    let stream = system
        .process_system()
        .spawn_stream::<EventEnvelope<OrderPlaced>>(ProcessName::new("saga-replay-stream"))
        .await
        .expect("spawn_stream must succeed");

    let saga_id = SagaId::new("saga-replay-direct");
    let captured: Arc<Mutex<Vec<SagaId>>> = Arc::new(Mutex::new(Vec::new()));
    let done = Arc::new(Notify::new());

    let captured_first = Arc::clone(&captured);
    let done_first = Arc::clone(&done);
    let routed = saga_id.clone();
    let route_fn = move |_event: &OrderPlaced| -> Option<SagaId> { Some(routed.clone()) };

    let saga_proxy = SagaProps::<RecordingSaga>::new(
        saga_id.clone(),
        Arc::clone(&store),
        move || RecordingSaga {
            captured: Arc::clone(&captured_first),
            done: Arc::clone(&done_first),
        },
    )
    .with_codec(system.codec::<ReservationRequested>())
    .with_subscription(stream.clone(), route_fn.clone())
    .spawn(system.process_system())
    .await;

    order_proxy
        .ask(PlaceOrder {
            sku: "SKU-replay-1".into(),
            stream: stream.clone(),
        })
        .await
        .expect("ask must succeed");
    tokio::time::timeout(Duration::from_millis(500), done.notified())
        .await
        .expect("first persist must complete");
    tokio::time::sleep(Duration::from_millis(50)).await;

    drop(saga_proxy);

    // When: re-spawn the saga with the same id and the same store
    let done2 = Arc::new(Notify::new());
    let captured2 = Arc::clone(&captured);
    let done2_for_producer = Arc::clone(&done2);

    let _saga_proxy2 = SagaProps::<RecordingSaga>::new(
        saga_id.clone(),
        Arc::clone(&store),
        move || RecordingSaga {
            captured: Arc::clone(&captured2),
            done: Arc::clone(&done2_for_producer),
        },
    )
    .with_codec(system.codec::<ReservationRequested>())
    .with_subscription(stream.clone(), route_fn)
    .spawn(system.process_system())
    .await;

    order_proxy
        .ask(PlaceOrder {
            sku: "SKU-replay-2".into(),
            stream: stream.clone(),
        })
        .await
        .expect("ask must succeed");
    tokio::time::timeout(Duration::from_millis(500), done2.notified())
        .await
        .expect("second persist must complete");
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Then: the saga stream now contains 2 events at sequences 1 and 2 —
    // proving on_start replayed sequence 1 from the direct store before
    // accepting the next handle.
    let saga_events: Vec<_> = store
        .load(LoadQuery::by_stream(&saga_id))
        .await
        .expect("load must succeed")
        .try_collect()
        .await
        .expect("collect must succeed");

    assert_eq!(
        saga_events.len(),
        2,
        "saga stream must contain both persisted events after replay-then-persist"
    );
    assert_eq!(saga_events[0].sequence, 1);
    assert_eq!(
        saga_events[1].sequence, 2,
        "second persist must continue at sequence 2, proving replay restored state"
    );
}
