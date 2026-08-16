//! The manager owns exactly one upstream subscription, so each upstream record
//! is decoded once no matter how many saga instances the correlation fans out
//! to.
//!
//! Before the manager existed, every instance ran its own `DirectPollerProcess`
//! over the same upstream store, so `M` events across `N` instances cost `M × N`
//! decodes.  The counter below is a process-wide `static` (a `Codec` is a pair
//! of associated functions and cannot carry instance state), which is why this
//! binary deliberately holds a single test.

#[path = "common/helpers.rs"]
mod common;
use common::JsonCodec;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use serde::{Deserialize, Serialize};
use tokio::sync::Notify;

use nitinol_eventsource::codec::{Codec, ErasedCodec};
use nitinol_eventsource::{system::EventSourceSystem, Event, SequenceCursor};
use nitinol_persistence::store::{EventStore, InMemoryEventStore};
use nitinol_persistence::{AggregateId, AppendingEvent, EventType, Family, TypeName};
use nitinol_runtime::ProcessSystem;
use nitinol_saga::{Saga, SagaContext, SagaEffect, SagaId, SagaManagerProps};

const INSTANCE_PREFIX: &str = "mgr-single-";

/// The orders the upstream stream carries, in append order.  Three distinct
/// correlation ids (`N = 3`) across six records (`M = 6`).
const UPSTREAM_ORDERS: [&str; 6] = ["A", "B", "C", "A", "B", "C"];

/// Number of decodes the upstream codec performed.  `static` because
/// [`Codec::decode`] is an associated function with no receiver to hang state
/// off; the single test in this binary owns it exclusively.
static UPSTREAM_DECODES: AtomicUsize = AtomicUsize::new(0);

// Domain types

#[derive(Clone, Debug, Serialize, Deserialize)]
struct OrderPlaced {
    order: String,
}

impl Event for OrderPlaced {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("mgr.single"), TypeName::new("OrderPlaced"));
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ReservationRequested {
    order: String,
}

impl Event for ReservationRequested {
    const EVENT_TYPE: EventType = EventType::new(
        Family::new("mgr.single"),
        TypeName::new("ReservationRequested"),
    );
}

/// JSON codec for the upstream event type that counts every decode.  Bound to
/// `OrderPlaced` alone so unrelated decodes cannot inflate the counter.
struct CountingUpstreamCodec;

impl Codec<OrderPlaced> for CountingUpstreamCodec {
    type Error = serde_json::Error;

    fn encode(event: &OrderPlaced) -> Result<Bytes, Self::Error> {
        serde_json::to_vec(event).map(Bytes::from)
    }

    fn decode(payload: &[u8]) -> Result<OrderPlaced, Self::Error> {
        UPSTREAM_DECODES.fetch_add(1, Ordering::SeqCst);
        serde_json::from_slice(payload)
    }
}

// Saga under test

struct RecordingSaga {
    handled: Arc<Mutex<Vec<String>>>,
    notify: Arc<Notify>,
}

#[async_trait]
impl Saga for RecordingSaga {
    type SubscribedEvent = OrderPlaced;
    type Event = ReservationRequested;
    type ScheduledMessage = ();
    type Error = std::convert::Infallible;

    fn correlate(event: &Self::SubscribedEvent) -> Option<SagaId> {
        Some(SagaId::new(format!("{INSTANCE_PREFIX}{}", event.order)))
    }

    fn apply(&mut self, _event: Self::Event) {}

    async fn handle(
        &mut self,
        _event: Self::SubscribedEvent,
        ctx: &mut SagaContext,
    ) -> Result<SagaEffect<Self::Event>, Self::Error> {
        self.handled
            .lock()
            .expect("handled mutex is never poisoned: no holder panics while the guard is alive")
            .push(ctx.saga_id().as_str().to_owned());
        self.notify.notify_one();
        Ok(SagaEffect::None)
    }
}

// Helpers

async fn append_order_placed(
    store: &Arc<dyn EventStore>,
    upstream_key: &AggregateId,
    sequence: u64,
    order: &str,
) {
    let payload = serde_json::to_vec(&OrderPlaced {
        order: order.to_owned(),
    })
    .map(Bytes::from)
    .expect("encode OrderPlaced must succeed");
    store
        .append(
            upstream_key.as_str(),
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

fn handled_snapshot(handled: &Arc<Mutex<Vec<String>>>) -> Vec<String> {
    handled
        .lock()
        .expect("handled mutex is never poisoned: no holder panics while the guard is alive")
        .clone()
}

async fn wait_for_handled(
    handled: &Arc<Mutex<Vec<String>>>,
    notify: &Arc<Notify>,
    expected: usize,
) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let notified = notify.notified();
            if handled_snapshot(handled).len() >= expected {
                return;
            }
            notified.await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "timed out waiting for {expected} handle() calls (got {:?})",
            handled_snapshot(handled)
        )
    });
}

// Test

#[tokio::test]
async fn upstream_decode_happens_once_per_event_regardless_of_instance_count() {
    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::new(ps).with_codec::<JsonCodec>().build();

    let upstream_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let upstream_key = AggregateId::new("mgr-single-orders");
    for (index, order) in UPSTREAM_ORDERS.iter().enumerate() {
        let sequence = u64::try_from(index + 1).expect("test sequence fits in u64");
        append_order_placed(&upstream_store, &upstream_key, sequence, order).await;
    }

    let saga_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());

    let handled: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let notify = Arc::new(Notify::new());
    let instances_spawned = Arc::new(AtomicUsize::new(0));

    let handled_for_producer = Arc::clone(&handled);
    let notify_for_producer = Arc::clone(&notify);
    let spawned_for_producer = Arc::clone(&instances_spawned);

    let _manager_proxy =
        SagaManagerProps::<RecordingSaga>::new(Arc::clone(&saga_store), move || {
            spawned_for_producer.fetch_add(1, Ordering::SeqCst);
            RecordingSaga {
                handled: Arc::clone(&handled_for_producer),
                notify: Arc::clone(&notify_for_producer),
            }
        })
        .with_codec(system.codec::<ReservationRequested>())
        .with_subscription(
            Arc::clone(&upstream_store),
            Arc::new(CountingUpstreamCodec) as Arc<dyn ErasedCodec<OrderPlaced>>,
            SequenceCursor::Stream {
                key: upstream_key.as_str().to_owned(),
                after: 0,
            },
        )
        .spawn(system.process_system())
        .await;

    wait_for_handled(&handled, &notify, UPSTREAM_ORDERS.len()).await;
    // A per-instance poller would keep decoding after the last delivery, so
    // settle past one poll interval before reading the counter.
    tokio::time::sleep(Duration::from_millis(500)).await;

    let mut instance_ids = handled_snapshot(&handled);
    instance_ids.sort();
    instance_ids.dedup();
    assert_eq!(
        instance_ids,
        vec![
            format!("{INSTANCE_PREFIX}A"),
            format!("{INSTANCE_PREFIX}B"),
            format!("{INSTANCE_PREFIX}C"),
        ],
        "the six upstream records must fan out to the three instances \
         Saga::correlate names"
    );
    assert_eq!(
        instances_spawned.load(Ordering::SeqCst),
        3,
        "three distinct correlation ids must produce three instances — without \
         them the decode count below would not be testing fan-out at all"
    );
    assert_eq!(
        UPSTREAM_DECODES.load(Ordering::SeqCst),
        UPSTREAM_ORDERS.len(),
        "the manager must decode each upstream record exactly once; \
         one subscription per instance would decode {} × {} times",
        UPSTREAM_ORDERS.len(),
        3
    );
}
