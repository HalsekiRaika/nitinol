//! Manager-driven lazy spawn and instance reuse.
//!
//! `SagaManagerProps` holds one upstream subscription and interprets
//! `Saga::correlate` at runtime: an event naming an instance the manager has
//! never seen spawns that instance, which replays its own stream before the
//! event is delivered; an event naming an instance that is already running is
//! handed to it without a second spawn.

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

use nitinol_eventsource::{system::EventSourceSystem, Event, SequenceCursor};
use nitinol_persistence::store::{EventStore, InMemoryEventStore};
use nitinol_persistence::{AggregateId, AppendingEvent, EventType, Family, TypeName};
use nitinol_runtime::ProcessSystem;
use nitinol_saga::{Saga, SagaContext, SagaEffect, SagaId, SagaManagerProps};

/// Prefix of every instance id [`TracingSaga::correlate`] derives.  The order
/// key is appended verbatim, so one upstream stream fans out to one instance
/// per order.
const INSTANCE_PREFIX: &str = "mgr-lazy-";

// Domain types

#[derive(Clone, Debug, Serialize, Deserialize)]
struct OrderPlaced {
    order: String,
}

impl Event for OrderPlaced {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("mgr.lazy"), TypeName::new("OrderPlaced"));
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ReservationRequested {
    order: String,
}

impl Event for ReservationRequested {
    const EVENT_TYPE: EventType = EventType::new(
        Family::new("mgr.lazy"),
        TypeName::new("ReservationRequested"),
    );
}

// Saga under test

/// Records `apply` (driven by replay of the instance's own stream) and `handle`
/// (driven by upstream delivery) into one shared trace, so their relative order
/// is observable.
struct TracingSaga {
    trace: Arc<Mutex<Vec<String>>>,
    notify: Arc<Notify>,
}

impl TracingSaga {
    fn record(&self, entry: String) {
        self.trace
            .lock()
            .expect("trace mutex is never poisoned: no holder panics while the guard is alive")
            .push(entry);
        self.notify.notify_one();
    }
}

#[async_trait]
impl Saga for TracingSaga {
    type SubscribedEvent = OrderPlaced;
    type Event = ReservationRequested;
    type ScheduledMessage = ();
    type Error = std::convert::Infallible;

    fn correlate(event: &Self::SubscribedEvent) -> Option<SagaId> {
        Some(SagaId::new(format!("{INSTANCE_PREFIX}{}", event.order)))
    }

    fn apply(&mut self, event: Self::Event) {
        self.record(format!("apply:{}", event.order));
    }

    async fn handle(
        &mut self,
        event: Self::SubscribedEvent,
        ctx: &mut SagaContext,
    ) -> Result<SagaEffect<Self::Event>, Self::Error> {
        self.record(format!("handle:{}:{}", ctx.saga_id().as_str(), event.order));
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

/// Seed one of the saga's *own* domain events so the instance the manager
/// lazily spawns has something to replay before the upstream event reaches it.
async fn seed_saga_domain_event(
    store: &Arc<dyn EventStore>,
    saga_id: &SagaId,
    sequence: u64,
    order: &str,
) {
    let payload = serde_json::to_vec(&ReservationRequested {
        order: order.to_owned(),
    })
    .map(Bytes::from)
    .expect("encode ReservationRequested must succeed");
    store
        .append(
            saga_id.as_str(),
            vec![AppendingEvent {
                sequence,
                event_type: ReservationRequested::EVENT_TYPE,
                payload,
                occurred_at: jiff::Timestamp::now(),
            }],
        )
        .await
        .expect("seed saga domain event must succeed");
}

fn trace_snapshot(trace: &Arc<Mutex<Vec<String>>>) -> Vec<String> {
    trace
        .lock()
        .expect("trace mutex is never poisoned: no holder panics while the guard is alive")
        .clone()
}

async fn wait_for_trace_len(
    trace: &Arc<Mutex<Vec<String>>>,
    notify: &Arc<Notify>,
    expected: usize,
) {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let notified = notify.notified();
            if trace_snapshot(trace).len() >= expected {
                return;
            }
            notified.await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "timed out waiting for {expected} trace entries (got {:?})",
            trace_snapshot(trace)
        )
    });
}

// Tests

/// The instance for order `A` exists only as a durable
/// stream — no process was ever spawned for it.  The first upstream event that
/// correlates to it must spawn it, replay its stream, and only then deliver.
#[tokio::test]
async fn unknown_correlation_id_lazy_spawns_and_delivers_after_replay() {
    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::builder(ps)
        .with_codec::<JsonCodec>()
        .build();

    let upstream_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let upstream_key = AggregateId::new("mgr-lazy-spawn-orders");

    let saga_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let instance_id = SagaId::new(format!("{INSTANCE_PREFIX}A"));
    seed_saga_domain_event(&saga_store, &instance_id, 1, "A").await;

    let trace: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let notify = Arc::new(Notify::new());

    let trace_for_producer = Arc::clone(&trace);
    let notify_for_producer = Arc::clone(&notify);

    let _manager_proxy =
        SagaManagerProps::<TracingSaga>::new(Arc::clone(&saga_store), move || TracingSaga {
            trace: Arc::clone(&trace_for_producer),
            notify: Arc::clone(&notify_for_producer),
        })
        .with_codec(system.codec::<ReservationRequested>())
        .with_subscription(
            Arc::clone(&upstream_store),
            system.codec::<OrderPlaced>(),
            SequenceCursor::Stream {
                key: upstream_key.as_str().to_owned(),
                after: 0,
            },
        )
        .spawn(system.process_system())
        .await;

    append_order_placed(&upstream_store, &upstream_key, 1, "A").await;

    wait_for_trace_len(&trace, &notify, 2).await;

    assert_eq!(
        trace_snapshot(&trace),
        vec!["apply:A".to_owned(), format!("handle:{INSTANCE_PREFIX}A:A")],
        "an event correlating to an id with no running instance must lazily spawn \
         that instance, replay its own stream (apply) and only then reach handle(); \
         the handled instance id must be the one Saga::correlate named"
    );
}

/// The second event for a correlation id already in the
/// registry must reach the running instance — no second spawn, hence no second
/// replay.
#[tokio::test]
async fn known_correlation_id_reuses_the_running_instance() {
    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::builder(ps)
        .with_codec::<JsonCodec>()
        .build();

    let upstream_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let upstream_key = AggregateId::new("mgr-lazy-reuse-orders");

    let saga_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());

    let trace: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let notify = Arc::new(Notify::new());
    let instances_spawned = Arc::new(AtomicUsize::new(0));

    let trace_for_producer = Arc::clone(&trace);
    let notify_for_producer = Arc::clone(&notify);
    let spawned_for_producer = Arc::clone(&instances_spawned);

    let _manager_proxy = SagaManagerProps::<TracingSaga>::new(Arc::clone(&saga_store), move || {
        spawned_for_producer.fetch_add(1, Ordering::SeqCst);
        TracingSaga {
            trace: Arc::clone(&trace_for_producer),
            notify: Arc::clone(&notify_for_producer),
        }
    })
    .with_codec(system.codec::<ReservationRequested>())
    .with_subscription(
        Arc::clone(&upstream_store),
        system.codec::<OrderPlaced>(),
        SequenceCursor::Stream {
            key: upstream_key.as_str().to_owned(),
            after: 0,
        },
    )
    .spawn(system.process_system())
    .await;

    append_order_placed(&upstream_store, &upstream_key, 1, "B").await;
    append_order_placed(&upstream_store, &upstream_key, 2, "B").await;

    wait_for_trace_len(&trace, &notify, 2).await;
    // A per-event respawn would land after the second handle, so settle past one
    // poll interval before reading the spawn counter.
    tokio::time::sleep(Duration::from_millis(400)).await;

    assert_eq!(
        instances_spawned.load(Ordering::SeqCst),
        1,
        "two events correlating to the same id must be served by one instance; \
         a second spawn would mean the registry hit path is missing"
    );
    assert_eq!(
        trace_snapshot(&trace),
        vec![format!("handle:{INSTANCE_PREFIX}B:B"); 2],
        "both events must reach handle() on the same instance, and no replay \
         (apply) must run for the second event"
    );
}
