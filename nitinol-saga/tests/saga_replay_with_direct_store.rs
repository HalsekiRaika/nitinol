#[path = "common/helpers.rs"]
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
    system::EventSourceSystem, Aggregate, Decider, Decision, Event, SequenceCursor,
};
use nitinol_persistence::store::{EventStore, InMemoryEventStore};
use nitinol_persistence::{AggregateId, EventType, Family, LoadQuery, LoadedEvent, TypeName};
use nitinol_runtime::ProcessSystem;
use nitinol_saga::{Saga, SagaContext, SagaEffect, SagaId, SagaProps};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
struct OrderPlaced {
    sku: String,
}

impl Event for OrderPlaced {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("e2e.direct"), TypeName::new("OrderPlaced"));
    type Codec = JsonCodec;
}

#[derive(Default)]
struct Order;

impl Aggregate for Order {
    type Event = OrderPlaced;
    fn apply(&mut self, _event: OrderPlaced) {}
}

struct PlaceOrder {
    sku: String,
}

impl Decider<PlaceOrder> for Order {
    type Output = ();
    type Rejection = std::convert::Infallible;

    fn decide(&self, cmd: PlaceOrder) -> Decision<OrderPlaced, (), Self::Rejection> {
        Decision::persist(vec![OrderPlaced { sku: cmd.sku }]).output(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ReservationRequested {
    sku: String,
}

impl Event for ReservationRequested {
    const EVENT_TYPE: EventType = EventType::new(
        Family::new("e2e.direct"),
        TypeName::new("ReservationRequested"),
    );
    type Codec = JsonCodec;
}

/// Correlation rule of [`RecordingSaga`]: every `OrderPlaced` belongs to the
/// one recording instance each test in this file spawns against its own store.
const RECORDING_SAGA_ID: &str = "direct-store-saga-1";

struct RecordingSaga {
    captured: Arc<Mutex<Vec<SagaId>>>,
    done: Arc<Notify>,
}

#[async_trait]
impl Saga for RecordingSaga {
    type SubscribedEvent = OrderPlaced;
    type Event = ReservationRequested;
    type ScheduledMessage = ();
    type Error = std::convert::Infallible;

    fn correlate(_event: &Self::SubscribedEvent) -> Option<SagaId> {
        Some(SagaId::new(RECORDING_SAGA_ID))
    }

    fn apply(&mut self, _event: Self::Event) {}

    async fn handle(
        &mut self,
        event: Self::SubscribedEvent,
        ctx: &mut SagaContext,
    ) -> Result<SagaEffect<Self::Event>, Self::Error> {
        self.captured
            .lock()
            .expect("captured mutex is never poisoned: no holder panics while the guard is alive")
            .push(ctx.saga_id().clone());
        let notify = Arc::clone(&self.done);
        let effect = SagaEffect::persist(ReservationRequested { sku: event.sku });
        notify.notify_one();
        Ok(effect)
    }
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

/// The saga's own decisions, told apart from the framework bookkeeping that
/// shares their stream — the consumed-position record that rides the append
/// closing an input out, which for this file's single-`Persist` saga is the
/// same append its decision is in.
fn decisions(events: &[LoadedEvent]) -> Vec<&LoadedEvent> {
    events
        .iter()
        .filter(|e| e.event_type == ReservationRequested::EVENT_TYPE)
        .collect()
}

/// Wait for `expected` *decisions*, not for `expected` rows: a decision no
/// longer occupies its stream alone, so counting rows would return while the
/// saga still owes a decision.
async fn wait_for_decisions(
    store: &Arc<dyn EventStore>,
    saga_id: &SagaId,
    expected: usize,
    timeout: Duration,
) -> Vec<LoadedEvent> {
    tokio::time::timeout(timeout, async {
        loop {
            let events = load_saga_events(store, saga_id).await;
            if decisions(&events).len() >= expected {
                return events;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {expected} saga decisions"))
}

#[tokio::test]
async fn aggregate_and_saga_share_one_arc_dyn_event_store() {
    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::builder(ps).build();

    let store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());

    let order_id = AggregateId::new("direct-order");
    let order_proxy = system
        .spawn_aggregate::<Order>(order_id.clone(), Arc::clone(&store))
        .await;

    let saga_id = SagaId::new(RECORDING_SAGA_ID);
    let captured: Arc<Mutex<Vec<SagaId>>> = Arc::new(Mutex::new(Vec::new()));
    let done = Arc::new(Notify::new());

    let captured_for_producer = Arc::clone(&captured);
    let done_for_producer = Arc::clone(&done);

    let _saga_proxy =
        SagaProps::<RecordingSaga>::new(saga_id.clone(), Arc::clone(&store), move || {
            RecordingSaga {
                captured: Arc::clone(&captured_for_producer),
                done: Arc::clone(&done_for_producer),
            }
        })
        .with_subscription(
            Arc::clone(&store),
            SequenceCursor::Stream {
                key: order_id.as_str().to_owned(),
                after: 0,
            },
        )
        .spawn(system.process_system())
        .await;

    order_proxy
        .ask(PlaceOrder {
            sku: "SKU-direct-1".into(),
        })
        .await
        .expect("ask(PlaceOrder) must succeed");

    tokio::time::timeout(Duration::from_secs(3), done.notified())
        .await
        .expect("saga must persist within 3 seconds");

    tokio::time::sleep(Duration::from_millis(50)).await;

    let saga_events = load_saga_events(&store, &saga_id).await;
    let persisted = decisions(&saga_events);

    assert_eq!(
        persisted.len(),
        1,
        "saga must persist exactly one ReservationRequested event"
    );
    assert_eq!(persisted[0].sequence, 1);
    assert_eq!(
        persisted[0].stream_key,
        saga_id.as_str(),
        "saga event must be keyed by saga_id (Borrow<str>) — not by an aggregate id"
    );

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

    let captured = captured
        .lock()
        .expect("captured mutex is never poisoned: no holder panics while the guard is alive");
    assert_eq!(captured.len(), 1);
    assert_eq!(captured[0].as_str(), RECORDING_SAGA_ID);
}

/// Regression test: dropping all `SagaProxy` handles must NOT stop the
/// upstream `DurableStream` subscription.  The subscription lifetime is owned by `SagaProcess` itself,
/// so the process continues receiving and persisting events after every handle
/// has been released.
#[tokio::test]
async fn saga_proxy_drop_does_not_stop_upstream_subscription() {
    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::builder(ps).build();

    let store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());

    let order_id = AggregateId::new("drop-proxy-order");
    let order_proxy = system
        .spawn_aggregate::<Order>(order_id.clone(), Arc::clone(&store))
        .await;

    let saga_id = SagaId::new(RECORDING_SAGA_ID);
    let captured: Arc<Mutex<Vec<SagaId>>> = Arc::new(Mutex::new(Vec::new()));
    let done = Arc::new(Notify::new());

    let captured_for_producer = Arc::clone(&captured);
    let done_for_producer = Arc::clone(&done);

    let saga_proxy =
        SagaProps::<RecordingSaga>::new(saga_id.clone(), Arc::clone(&store), move || {
            RecordingSaga {
                captured: Arc::clone(&captured_for_producer),
                done: Arc::clone(&done_for_producer),
            }
        })
        .with_subscription(
            Arc::clone(&store),
            SequenceCursor::Stream {
                key: order_id.as_str().to_owned(),
                after: 0,
            },
        )
        .spawn(system.process_system())
        .await;

    order_proxy
        .ask(PlaceOrder {
            sku: "SKU-drop-1".into(),
        })
        .await
        .expect("ask(PlaceOrder) must succeed");
    wait_for_decisions(&store, &saga_id, 1, Duration::from_secs(3)).await;

    drop(saga_proxy);

    order_proxy
        .ask(PlaceOrder {
            sku: "SKU-drop-2".into(),
        })
        .await
        .expect("ask(PlaceOrder) must succeed");

    let saga_events = wait_for_decisions(&store, &saga_id, 2, Duration::from_secs(3)).await;
    let persisted = decisions(&saga_events);

    assert_eq!(
        persisted.len(),
        2,
        "SagaProxy drop must not stop the upstream subscription; \
         SagaProcess must continue persisting events after the handle is released"
    );
    // Each decision is committed with the position of the input it was decided
    // from, so the second one starts a sequence past its predecessor's pair.
    assert_eq!(persisted[0].sequence, 1);
    assert_eq!(persisted[1].sequence, 3);
}

#[tokio::test]
async fn saga_replays_its_own_stream_via_direct_store_on_respawn() {
    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::builder(ps).build();
    let store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());

    let order_id = AggregateId::new("replay-order");
    let order_proxy = system
        .spawn_aggregate::<Order>(order_id.clone(), Arc::clone(&store))
        .await;

    let saga_id = SagaId::new(RECORDING_SAGA_ID);
    let captured: Arc<Mutex<Vec<SagaId>>> = Arc::new(Mutex::new(Vec::new()));
    let done = Arc::new(Notify::new());

    let captured_first = Arc::clone(&captured);
    let done_first = Arc::clone(&done);

    let saga_proxy =
        SagaProps::<RecordingSaga>::new(saga_id.clone(), Arc::clone(&store), move || {
            RecordingSaga {
                captured: Arc::clone(&captured_first),
                done: Arc::clone(&done_first),
            }
        })
        .with_subscription(
            Arc::clone(&store),
            SequenceCursor::Stream {
                key: order_id.as_str().to_owned(),
                after: 0,
            },
        )
        .spawn(system.process_system())
        .await;

    order_proxy
        .ask(PlaceOrder {
            sku: "SKU-replay-1".into(),
        })
        .await
        .expect("ask must succeed");
    wait_for_decisions(&store, &saga_id, 1, Duration::from_secs(3)).await;

    // Explicitly stop the saga process so the upstream DurableStream poller is
    // also torn down.  Dropping the proxy handle alone is not sufficient —
    // the process is owned by the runtime, not by the handle.
    saga_proxy.stop().await.expect("stop must succeed");
    drop(saga_proxy);

    let done2 = Arc::new(Notify::new());
    let captured2 = Arc::clone(&captured);
    let done2_for_producer = Arc::clone(&done2);

    let _saga_proxy2 =
        SagaProps::<RecordingSaga>::new(saga_id.clone(), Arc::clone(&store), move || {
            RecordingSaga {
                captured: Arc::clone(&captured2),
                done: Arc::clone(&done2_for_producer),
            }
        })
        .with_subscription(
            Arc::clone(&store),
            SequenceCursor::Stream {
                key: order_id.as_str().to_owned(),
                after: 0,
            },
        )
        .spawn(system.process_system())
        .await;

    order_proxy
        .ask(PlaceOrder {
            sku: "SKU-replay-2".into(),
        })
        .await
        .expect("ask must succeed");

    let saga_events = wait_for_decisions(&store, &saga_id, 2, Duration::from_secs(3)).await;

    let seen = captured
        .lock()
        .expect("captured mutex is never poisoned: no holder panics while the guard is alive")
        .len();
    assert_eq!(
        seen, 2,
        "one handle call per upstream order: the catchup the respawn performs \
         re-reads the order the first incarnation already decided from, and the \
         position replay restored is what keeps it from reaching handle a \
         second time"
    );

    let persisted = decisions(&saga_events);
    assert_eq!(
        persisted.len(),
        2,
        "one decision per order — one written before the drop and one after the \
         respawn"
    );
    assert_eq!(persisted[0].sequence, 1);
    assert_eq!(
        persisted[1].sequence, 3,
        "the respawned incarnation appends past the pair the first one left \
         behind, which is what proves on_start restored state.sequence to 2 \
         from the direct store"
    );
    assert_eq!(
        saga_events.last().map(|e| e.sequence),
        Some(4),
        "the position committed with the second decision is the stream's last \
         row"
    );
}
