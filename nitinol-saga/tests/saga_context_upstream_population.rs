#[path = "common/helpers.rs"]
mod common;
use common::JsonCodec;

use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use serde::{Deserialize, Serialize};
use tokio::sync::Notify;

use nitinol_eventsource::{system::EventSourceSystem, Event, SequenceCursor};
use nitinol_persistence::store::{EventStore, InMemoryEventStore};
use nitinol_persistence::{AggregateId, AppendingEvent, EventType, Family, TypeName};
use nitinol_runtime::ProcessSystem;
use nitinol_saga::{Saga, SagaContext, SagaEffect, SagaId, SagaProps};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
struct OrderPlaced {
    sku: String,
}

impl Event for OrderPlaced {
    const EVENT_TYPE: EventType = EventType::new(
        Family::new("saga.ctx.upstream"),
        TypeName::new("OrderPlaced"),
    );
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ReservationRequested {
    sku: String,
}

impl Event for ReservationRequested {
    const EVENT_TYPE: EventType = EventType::new(
        Family::new("saga.ctx.upstream"),
        TypeName::new("ReservationRequested"),
    );
}

#[derive(Debug, Clone)]
struct CapturedUpstream {
    saga_id: SagaId,
    saga_sequence: u64,
    upstream_aggregate_id: AggregateId,
    upstream_sequence: u64,
    now: jiff::Timestamp,
}

struct CapturingSaga {
    captured: Arc<Mutex<Vec<CapturedUpstream>>>,
    notify: Arc<Notify>,
}

#[async_trait]
impl Saga for CapturingSaga {
    type SubscribedEvent = OrderPlaced;
    type Event = ReservationRequested;
    type State = ();
    type ScheduledMessage = ();
    type Error = std::convert::Infallible;

    fn apply(&mut self, _event: Self::Event) {}

    async fn handle(
        &mut self,
        _event: Self::SubscribedEvent,
        ctx: &mut SagaContext,
    ) -> Result<SagaEffect<Self::Event>, Self::Error> {
        self.captured
            .lock()
            .expect("captured mutex is never poisoned: no holder panics while the guard is alive")
            .push(CapturedUpstream {
                saga_id: ctx.saga_id().clone(),
                saga_sequence: ctx.sequence(),
                upstream_aggregate_id: ctx.upstream_aggregate_id().clone(),
                upstream_sequence: ctx.upstream_sequence(),
                now: ctx.now(),
            });
        self.notify.notify_one();
        Ok(SagaEffect::None)
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

async fn wait_for_count(
    captured: &Arc<Mutex<Vec<CapturedUpstream>>>,
    notify: &Arc<Notify>,
    expected: usize,
) {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let notified = notify.notified();
            if captured
                .lock()
                .expect(
                    "captured mutex is never poisoned: no holder panics while the guard is alive",
                )
                .len()
                >= expected
            {
                return;
            }
            notified.await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "timed out waiting for {expected} handle() calls (got {})",
            captured
                .lock()
                .expect(
                    "captured mutex is never poisoned: no holder panics while the guard is alive"
                )
                .len()
        )
    });
}

#[tokio::test]
async fn saga_context_exposes_upstream_aggregate_id_from_envelope() {
    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::new(ps).with_codec::<JsonCodec>().build();
    let upstream_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let order_id = AggregateId::new("saga-ctx-upstream-order");

    append_order_placed(&upstream_store, &order_id, 1, "SKU-1").await;

    let saga_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let saga_id = SagaId::new("saga-ctx-upstream-1");
    let captured: Arc<Mutex<Vec<CapturedUpstream>>> = Arc::new(Mutex::new(Vec::new()));
    let notify = Arc::new(Notify::new());

    let captured_for_producer = Arc::clone(&captured);
    let notify_for_producer = Arc::clone(&notify);

    let routed = saga_id.clone();
    let route_fn = move |_event: &OrderPlaced| -> Option<SagaId> { Some(routed.clone()) };

    let _saga_proxy =
        SagaProps::<CapturingSaga>::new(saga_id.clone(), saga_store, move || CapturingSaga {
            captured: Arc::clone(&captured_for_producer),
            notify: Arc::clone(&notify_for_producer),
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

    wait_for_count(&captured, &notify, 1).await;

    let captured = captured
        .lock()
        .expect("captured mutex is never poisoned: no holder panics while the guard is alive");
    assert_eq!(captured.len(), 1);
    assert_eq!(
        captured[0].upstream_aggregate_id, order_id,
        "ctx.upstream_aggregate_id() must equal the EventEnvelope's aggregate_id"
    );
}

#[tokio::test]
async fn saga_context_exposes_upstream_sequence_from_envelope() {
    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::new(ps).with_codec::<JsonCodec>().build();
    let upstream_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let order_id = AggregateId::new("saga-ctx-upstream-seq-order");

    append_order_placed(&upstream_store, &order_id, 1, "S-1").await;
    append_order_placed(&upstream_store, &order_id, 2, "S-2").await;
    append_order_placed(&upstream_store, &order_id, 3, "S-3").await;

    let saga_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let saga_id = SagaId::new("saga-ctx-upstream-seq-1");
    let captured: Arc<Mutex<Vec<CapturedUpstream>>> = Arc::new(Mutex::new(Vec::new()));
    let notify = Arc::new(Notify::new());

    let captured_for_producer = Arc::clone(&captured);
    let notify_for_producer = Arc::clone(&notify);

    let routed = saga_id.clone();
    let route_fn = move |_event: &OrderPlaced| -> Option<SagaId> { Some(routed.clone()) };

    let _saga_proxy =
        SagaProps::<CapturingSaga>::new(saga_id.clone(), saga_store, move || CapturingSaga {
            captured: Arc::clone(&captured_for_producer),
            notify: Arc::clone(&notify_for_producer),
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

    wait_for_count(&captured, &notify, 3).await;

    let captured = captured
        .lock()
        .expect("captured mutex is never poisoned: no holder panics while the guard is alive");
    let upstream_seqs: Vec<u64> = captured.iter().map(|c| c.upstream_sequence).collect();
    assert_eq!(
        upstream_seqs,
        vec![1, 2, 3],
        "ctx.upstream_sequence() must reflect the per-stream sequence of each envelope"
    );
}

#[tokio::test]
async fn saga_context_now_returns_runtime_timestamp_not_unix_epoch() {
    let started = jiff::Timestamp::now();

    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::new(ps).with_codec::<JsonCodec>().build();
    let upstream_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let order_id = AggregateId::new("saga-ctx-now-order");

    append_order_placed(&upstream_store, &order_id, 1, "NOW-1").await;

    let saga_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let saga_id = SagaId::new("saga-ctx-now-1");
    let captured: Arc<Mutex<Vec<CapturedUpstream>>> = Arc::new(Mutex::new(Vec::new()));
    let notify = Arc::new(Notify::new());

    let captured_for_producer = Arc::clone(&captured);
    let notify_for_producer = Arc::clone(&notify);

    let routed = saga_id.clone();
    let route_fn = move |_event: &OrderPlaced| -> Option<SagaId> { Some(routed.clone()) };

    let _saga_proxy =
        SagaProps::<CapturingSaga>::new(saga_id.clone(), saga_store, move || CapturingSaga {
            captured: Arc::clone(&captured_for_producer),
            notify: Arc::clone(&notify_for_producer),
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

    wait_for_count(&captured, &notify, 1).await;

    let captured = captured
        .lock()
        .expect("captured mutex is never poisoned: no holder panics while the guard is alive");
    let observed = captured[0].now;

    assert_ne!(
        observed,
        jiff::Timestamp::UNIX_EPOCH,
        "ctx.now() must not be the test_context default UNIX_EPOCH inside the runtime"
    );
    let diff_secs = (observed.as_second() - started.as_second()).unsigned_abs();
    assert!(
        diff_secs < 60,
        "ctx.now() must be close to the test start time (within 60s), \
         started={started}, observed={observed}"
    );
}

#[tokio::test]
async fn saga_context_existing_accessors_remain_saga_scoped() {
    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::new(ps).with_codec::<JsonCodec>().build();
    let upstream_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let order_id = AggregateId::new("saga-ctx-saga-scope-order");

    append_order_placed(&upstream_store, &order_id, 7, "SCOPE-1").await;

    let saga_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let saga_id = SagaId::new("saga-ctx-saga-scope-1");
    let captured: Arc<Mutex<Vec<CapturedUpstream>>> = Arc::new(Mutex::new(Vec::new()));
    let notify = Arc::new(Notify::new());

    let captured_for_producer = Arc::clone(&captured);
    let notify_for_producer = Arc::clone(&notify);

    let routed = saga_id.clone();
    let route_fn = move |_event: &OrderPlaced| -> Option<SagaId> { Some(routed.clone()) };

    let _saga_proxy =
        SagaProps::<CapturingSaga>::new(saga_id.clone(), saga_store, move || CapturingSaga {
            captured: Arc::clone(&captured_for_producer),
            notify: Arc::clone(&notify_for_producer),
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

    wait_for_count(&captured, &notify, 1).await;

    let captured = captured
        .lock()
        .expect("captured mutex is never poisoned: no holder panics while the guard is alive");
    assert_eq!(
        captured[0].saga_id, saga_id,
        "ctx.saga_id() must remain the saga instance id, not the upstream aggregate id"
    );
    assert_eq!(
        captured[0].saga_sequence, 0,
        "ctx.sequence() must remain the saga's own sequence (0 before any Persist) — not the upstream sequence"
    );
    assert_eq!(
        captured[0].upstream_sequence, 7,
        "ctx.upstream_sequence() must reflect the envelope's sequence (7), distinct from the saga's own sequence (0)"
    );
}
