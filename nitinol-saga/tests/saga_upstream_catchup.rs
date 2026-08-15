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
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("saga.upstream"), TypeName::new("OrderPlaced"));
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ReservationRequested {
    sku: String,
}

impl Event for ReservationRequested {
    const EVENT_TYPE: EventType = EventType::new(
        Family::new("saga.upstream"),
        TypeName::new("ReservationRequested"),
    );
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
struct UnrelatedEvent;

impl Event for UnrelatedEvent {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("saga.upstream"), TypeName::new("Unrelated"));
}

/// Correlation rule of [`RecordingSaga`]: every `OrderPlaced` belongs to the one
/// reservation process this file's catchup tests spawn.
const RECORDING_SAGA_ID: &str = "saga-upstream-catchup-recording";

/// Correlation rule of [`MatchOnlySaga`]: only orders whose SKU carries the
/// `MATCH-` prefix belong to it.
const MATCH_ONLY_SAGA_ID: &str = "saga-upstream-route-match";

fn record(captured: &Arc<Mutex<Vec<String>>>, notify: &Arc<Notify>, sku: String) {
    captured
        .lock()
        .expect("captured mutex is never poisoned: no holder panics while the guard is alive")
        .push(sku);
    notify.notify_one();
}

struct RecordingSaga {
    captured: Arc<Mutex<Vec<String>>>,
    notify: Arc<Notify>,
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
        _ctx: &mut SagaContext,
    ) -> Result<SagaEffect<Self::Event>, Self::Error> {
        record(&self.captured, &self.notify, event.sku);
        Ok(SagaEffect::None)
    }
}

/// A saga whose correlation declines part of the stream.  It is a separate type
/// from [`RecordingSaga`] because correlation now belongs to the type: the other
/// tests in this file stream SKUs that carry no `MATCH-` prefix, so folding this
/// rule into `RecordingSaga` would leave them with nothing delivered.
struct MatchOnlySaga {
    captured: Arc<Mutex<Vec<String>>>,
    notify: Arc<Notify>,
}

#[async_trait]
impl Saga for MatchOnlySaga {
    type SubscribedEvent = OrderPlaced;
    type Event = ReservationRequested;
    type ScheduledMessage = ();
    type Error = std::convert::Infallible;

    fn correlate(event: &Self::SubscribedEvent) -> Option<SagaId> {
        event
            .sku
            .starts_with("MATCH-")
            .then(|| SagaId::new(MATCH_ONLY_SAGA_ID))
    }

    fn apply(&mut self, _event: Self::Event) {}

    async fn handle(
        &mut self,
        event: Self::SubscribedEvent,
        _ctx: &mut SagaContext,
    ) -> Result<SagaEffect<Self::Event>, Self::Error> {
        record(&self.captured, &self.notify, event.sku);
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

async fn append_unrelated(store: &Arc<dyn EventStore>, agg_id: &AggregateId, sequence: u64) {
    let payload = serde_json::to_vec(&UnrelatedEvent)
        .map(Bytes::from)
        .expect("encode UnrelatedEvent must succeed");
    store
        .append(
            agg_id.as_str(),
            vec![AppendingEvent {
                sequence,
                event_type: UnrelatedEvent::EVENT_TYPE,
                payload,
                occurred_at: jiff::Timestamp::now(),
            }],
        )
        .await
        .expect("append UnrelatedEvent must succeed");
}

async fn wait_for_count(captured: &Arc<Mutex<Vec<String>>>, notify: &Arc<Notify>, expected: usize) {
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
async fn saga_catches_up_on_preexisting_upstream_events() {
    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::new(ps).with_codec::<JsonCodec>().build();
    let upstream_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let order_id = AggregateId::new("saga-upstream-catchup-order");

    append_order_placed(&upstream_store, &order_id, 1, "SKU-A").await;
    append_order_placed(&upstream_store, &order_id, 2, "SKU-B").await;
    append_order_placed(&upstream_store, &order_id, 3, "SKU-C").await;

    let saga_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let saga_id = SagaId::new(RECORDING_SAGA_ID);
    let captured: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let notify = Arc::new(Notify::new());

    let captured_for_producer = Arc::clone(&captured);
    let notify_for_producer = Arc::clone(&notify);

    let _saga_proxy =
        SagaProps::<RecordingSaga>::new(saga_id.clone(), saga_store, move || RecordingSaga {
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
        )
        .spawn(system.process_system())
        .await;

    wait_for_count(&captured, &notify, 3).await;

    let seen = captured
        .lock()
        .expect("captured mutex is never poisoned: no holder panics while the guard is alive")
        .clone();
    assert_eq!(
        seen,
        vec!["SKU-A".to_owned(), "SKU-B".to_owned(), "SKU-C".to_owned()],
        "the saga must catch up on every upstream event that pre-dates its spawn, in order"
    );
}

#[tokio::test]
async fn saga_catchup_declines_events_that_correlate_elsewhere() {
    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::new(ps).with_codec::<JsonCodec>().build();
    let upstream_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let order_id = AggregateId::new("saga-upstream-route-order");

    append_order_placed(&upstream_store, &order_id, 1, "SKIP-1").await;
    append_order_placed(&upstream_store, &order_id, 2, "MATCH-1").await;
    append_order_placed(&upstream_store, &order_id, 3, "SKIP-2").await;
    append_order_placed(&upstream_store, &order_id, 4, "MATCH-2").await;

    let saga_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let captured: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let notify = Arc::new(Notify::new());

    let captured_for_producer = Arc::clone(&captured);
    let notify_for_producer = Arc::clone(&notify);

    let _saga_proxy =
        SagaProps::<MatchOnlySaga>::new(SagaId::new(MATCH_ONLY_SAGA_ID), saga_store, move || {
            MatchOnlySaga {
                captured: Arc::clone(&captured_for_producer),
                notify: Arc::clone(&notify_for_producer),
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
        )
        .spawn(system.process_system())
        .await;

    wait_for_count(&captured, &notify, 2).await;
    tokio::time::sleep(Duration::from_millis(300)).await;

    let seen = captured
        .lock()
        .expect("captured mutex is never poisoned: no holder panics while the guard is alive")
        .clone();
    assert_eq!(
        seen,
        vec!["MATCH-1".to_owned(), "MATCH-2".to_owned()],
        "`correlate` must filter catchup events; the SKIP events correlate to \
         nobody and must never reach handle()"
    );
}

#[tokio::test]
async fn saga_catchup_resumes_from_cursor_after_value() {
    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::new(ps).with_codec::<JsonCodec>().build();
    let upstream_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let order_id = AggregateId::new("saga-upstream-resume-order");

    for (seq, sku) in [(1u64, "SKU-1"), (2, "SKU-2"), (3, "SKU-3"), (4, "SKU-4")] {
        append_order_placed(&upstream_store, &order_id, seq, sku).await;
    }

    let saga_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let saga_id = SagaId::new(RECORDING_SAGA_ID);
    let captured: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let notify = Arc::new(Notify::new());

    let captured_for_producer = Arc::clone(&captured);
    let notify_for_producer = Arc::clone(&notify);

    let _saga_proxy = SagaProps::<RecordingSaga>::new(saga_id, saga_store, move || RecordingSaga {
        captured: Arc::clone(&captured_for_producer),
        notify: Arc::clone(&notify_for_producer),
    })
    .with_codec(system.codec::<ReservationRequested>())
    .with_subscription(
        Arc::clone(&upstream_store),
        system.codec::<OrderPlaced>(),
        SequenceCursor::Stream {
            key: order_id.as_str().to_owned(),
            after: 2,
        },
    )
    .spawn(system.process_system())
    .await;

    wait_for_count(&captured, &notify, 2).await;
    tokio::time::sleep(Duration::from_millis(300)).await;

    let seen = captured
        .lock()
        .expect("captured mutex is never poisoned: no holder panics while the guard is alive")
        .clone();
    assert_eq!(
        seen,
        vec!["SKU-3".to_owned(), "SKU-4".to_owned()],
        "events at or below cursor.after must be skipped during catchup"
    );
}

#[tokio::test]
async fn saga_receives_live_events_after_catchup_via_durable_stream() {
    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::new(ps).with_codec::<JsonCodec>().build();
    let upstream_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let order_id = AggregateId::new("saga-upstream-live-order");

    append_order_placed(&upstream_store, &order_id, 1, "CATCHUP").await;

    let saga_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let saga_id = SagaId::new(RECORDING_SAGA_ID);
    let captured: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let notify = Arc::new(Notify::new());

    let captured_for_producer = Arc::clone(&captured);
    let notify_for_producer = Arc::clone(&notify);

    let _saga_proxy = SagaProps::<RecordingSaga>::new(saga_id, saga_store, move || RecordingSaga {
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
    )
    .spawn(system.process_system())
    .await;

    wait_for_count(&captured, &notify, 1).await;
    append_order_placed(&upstream_store, &order_id, 2, "LIVE").await;

    wait_for_count(&captured, &notify, 2).await;
    let seen = captured
        .lock()
        .expect("captured mutex is never poisoned: no holder panics while the guard is alive")
        .clone();
    assert_eq!(
        seen,
        vec!["CATCHUP".to_owned(), "LIVE".to_owned()],
        "catchup + live delivery must share the same DurableStream channel; the live event must arrive after the catchup event"
    );
}

#[tokio::test]
async fn saga_catchup_ignores_events_of_other_types() {
    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::new(ps).with_codec::<JsonCodec>().build();
    let upstream_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let order_id = AggregateId::new("saga-upstream-mixed-order");

    append_order_placed(&upstream_store, &order_id, 1, "ORDER-1").await;
    append_unrelated(&upstream_store, &order_id, 2).await;
    append_order_placed(&upstream_store, &order_id, 3, "ORDER-2").await;

    let saga_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let saga_id = SagaId::new(RECORDING_SAGA_ID);
    let captured: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let notify = Arc::new(Notify::new());

    let captured_for_producer = Arc::clone(&captured);
    let notify_for_producer = Arc::clone(&notify);

    let _saga_proxy = SagaProps::<RecordingSaga>::new(saga_id, saga_store, move || RecordingSaga {
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
    )
    .spawn(system.process_system())
    .await;

    wait_for_count(&captured, &notify, 2).await;
    tokio::time::sleep(Duration::from_millis(300)).await;

    let seen = captured
        .lock()
        .expect("captured mutex is never poisoned: no holder panics while the guard is alive")
        .clone();
    assert_eq!(
        seen,
        vec!["ORDER-1".to_owned(), "ORDER-2".to_owned()],
        "unrelated event types in the same upstream store must be filtered out by the saga's transform"
    );
}

#[tokio::test]
async fn saga_catchup_with_global_cursor_orders_across_aggregates() {
    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::new(ps).with_codec::<JsonCodec>().build();
    let upstream_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());

    let agg_a = AggregateId::new("saga-upstream-global-a");
    let agg_b = AggregateId::new("saga-upstream-global-b");

    append_order_placed(&upstream_store, &agg_a, 1, "A1").await;
    append_order_placed(&upstream_store, &agg_b, 1, "B1").await;
    append_order_placed(&upstream_store, &agg_a, 2, "A2").await;
    append_order_placed(&upstream_store, &agg_b, 2, "B2").await;

    let saga_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let saga_id = SagaId::new(RECORDING_SAGA_ID);
    let captured: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let notify = Arc::new(Notify::new());

    let captured_for_producer = Arc::clone(&captured);
    let notify_for_producer = Arc::clone(&notify);

    let _saga_proxy = SagaProps::<RecordingSaga>::new(saga_id, saga_store, move || RecordingSaga {
        captured: Arc::clone(&captured_for_producer),
        notify: Arc::clone(&notify_for_producer),
    })
    .with_codec(system.codec::<ReservationRequested>())
    .with_subscription(
        Arc::clone(&upstream_store),
        system.codec::<OrderPlaced>(),
        SequenceCursor::Global { after: 0 },
    )
    .spawn(system.process_system())
    .await;

    wait_for_count(&captured, &notify, 4).await;

    let seen = captured
        .lock()
        .expect("captured mutex is never poisoned: no holder panics while the guard is alive")
        .clone();
    assert_eq!(
        seen,
        vec![
            "A1".to_owned(),
            "B1".to_owned(),
            "A2".to_owned(),
            "B2".to_owned(),
        ],
        "Global cursor must deliver catchup events in ascending global_sequence order across aggregates"
    );
}
