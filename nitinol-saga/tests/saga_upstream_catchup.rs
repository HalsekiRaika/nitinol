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

use nitinol_eventsource::{system::EventSourceSystem, Event};
use nitinol_persistence::provider::PersistenceProvider;
use nitinol_persistence::store::{InMemoryEventStore, InMemoryStore, Journal};
use nitinol_persistence::{AggregateId, AppendingEvent, EventType, Family, TypeName};
use nitinol_runtime::ProcessSystem;
use nitinol_saga::{
    Saga, SagaContext, SagaEffect, SagaId, SagaManagerProps, SagaSubscribe, Subscription,
};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
struct OrderPlaced {
    sku: String,
}

impl Event for OrderPlaced {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("saga.upstream"), TypeName::new("OrderPlaced"));
    type Codec = JsonCodec;
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
    type Codec = JsonCodec;
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
struct UnrelatedEvent;

impl Event for UnrelatedEvent {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("saga.upstream"), TypeName::new("Unrelated"));
    type Codec = JsonCodec;
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

/// The whole alphabet both sagas in this file react to: one observed source, no
/// deadline.
#[derive(SagaSubscribe)]
enum Observed {
    Order(OrderPlaced),
}

struct RecordingSaga {
    captured: Arc<Mutex<Vec<String>>>,
    notify: Arc<Notify>,
}

#[async_trait]
impl Saga for RecordingSaga {
    type Subscribe = Observed;
    type Event = ReservationRequested;
    type Error = std::convert::Infallible;

    fn correlate(_input: &Self::Subscribe) -> Option<SagaId> {
        Some(SagaId::new(RECORDING_SAGA_ID))
    }

    async fn handle(
        &mut self,
        input: Self::Subscribe,
        _ctx: &mut SagaContext,
    ) -> Result<SagaEffect<Self::Event>, Self::Error> {
        let Observed::Order(event) = input;
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
    type Subscribe = Observed;
    type Event = ReservationRequested;
    type Error = std::convert::Infallible;

    fn correlate(input: &Self::Subscribe) -> Option<SagaId> {
        let Observed::Order(event) = input;
        event
            .sku
            .starts_with("MATCH-")
            .then(|| SagaId::new(MATCH_ONLY_SAGA_ID))
    }

    async fn handle(
        &mut self,
        input: Self::Subscribe,
        _ctx: &mut SagaContext,
    ) -> Result<SagaEffect<Self::Event>, Self::Error> {
        let Observed::Order(event) = input;
        record(&self.captured, &self.notify, event.sku);
        Ok(SagaEffect::None)
    }
}

async fn append_order_placed(store: &Journal, agg_id: &AggregateId, sequence: u64, sku: &str) {
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

async fn append_unrelated(store: &Journal, agg_id: &AggregateId, sequence: u64) {
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
    let system = EventSourceSystem::builder(ps)
        .with_persistence(PersistenceProvider::new(InMemoryStore::default()))
        .build();
    let upstream_store = Journal::from(InMemoryEventStore::default());
    let order_id = AggregateId::new("saga-upstream-catchup-order");

    append_order_placed(&upstream_store, &order_id, 1, "SKU-A").await;
    append_order_placed(&upstream_store, &order_id, 2, "SKU-B").await;
    append_order_placed(&upstream_store, &order_id, 3, "SKU-C").await;

    let saga_store = Journal::from(InMemoryEventStore::default());
    let captured: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let notify = Arc::new(Notify::new());

    let captured_for_producer = Arc::clone(&captured);
    let notify_for_producer = Arc::clone(&notify);

    let _saga_proxy =
        SagaManagerProps::<RecordingSaga>::new(saga_store, move |_components| RecordingSaga {
            captured: Arc::clone(&captured_for_producer),
            notify: Arc::clone(&notify_for_producer),
        })
        .observing::<OrderPlaced>(Subscription::stream(&upstream_store, &order_id))
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
    let system = EventSourceSystem::builder(ps)
        .with_persistence(PersistenceProvider::new(InMemoryStore::default()))
        .build();
    let upstream_store = Journal::from(InMemoryEventStore::default());
    let order_id = AggregateId::new("saga-upstream-route-order");

    append_order_placed(&upstream_store, &order_id, 1, "SKIP-1").await;
    append_order_placed(&upstream_store, &order_id, 2, "MATCH-1").await;
    append_order_placed(&upstream_store, &order_id, 3, "SKIP-2").await;
    append_order_placed(&upstream_store, &order_id, 4, "MATCH-2").await;

    let saga_store = Journal::from(InMemoryEventStore::default());
    let captured: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let notify = Arc::new(Notify::new());

    let captured_for_producer = Arc::clone(&captured);
    let notify_for_producer = Arc::clone(&notify);

    let _saga_proxy =
        SagaManagerProps::<MatchOnlySaga>::new(saga_store, move |_components| MatchOnlySaga {
            captured: Arc::clone(&captured_for_producer),
            notify: Arc::clone(&notify_for_producer),
        })
        .observing::<OrderPlaced>(Subscription::stream(&upstream_store, &order_id))
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
    let system = EventSourceSystem::builder(ps)
        .with_persistence(PersistenceProvider::new(InMemoryStore::default()))
        .build();
    let upstream_store = Journal::from(InMemoryEventStore::default());
    let order_id = AggregateId::new("saga-upstream-resume-order");

    for (seq, sku) in [(1u64, "SKU-1"), (2, "SKU-2"), (3, "SKU-3"), (4, "SKU-4")] {
        append_order_placed(&upstream_store, &order_id, seq, sku).await;
    }

    let saga_store = Journal::from(InMemoryEventStore::default());
    let captured: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let notify = Arc::new(Notify::new());

    let captured_for_producer = Arc::clone(&captured);
    let notify_for_producer = Arc::clone(&notify);

    let _saga_proxy =
        SagaManagerProps::<RecordingSaga>::new(saga_store, move |_components| RecordingSaga {
            captured: Arc::clone(&captured_for_producer),
            notify: Arc::clone(&notify_for_producer),
        })
        .observing::<OrderPlaced>(Subscription::stream(&upstream_store, &order_id).with_after(2))
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
    let system = EventSourceSystem::builder(ps)
        .with_persistence(PersistenceProvider::new(InMemoryStore::default()))
        .build();
    let upstream_store = Journal::from(InMemoryEventStore::default());
    let order_id = AggregateId::new("saga-upstream-live-order");

    append_order_placed(&upstream_store, &order_id, 1, "CATCHUP").await;

    let saga_store = Journal::from(InMemoryEventStore::default());
    let captured: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let notify = Arc::new(Notify::new());

    let captured_for_producer = Arc::clone(&captured);
    let notify_for_producer = Arc::clone(&notify);

    let _saga_proxy =
        SagaManagerProps::<RecordingSaga>::new(saga_store, move |_components| RecordingSaga {
            captured: Arc::clone(&captured_for_producer),
            notify: Arc::clone(&notify_for_producer),
        })
        .observing::<OrderPlaced>(Subscription::stream(&upstream_store, &order_id))
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
    let system = EventSourceSystem::builder(ps)
        .with_persistence(PersistenceProvider::new(InMemoryStore::default()))
        .build();
    let upstream_store = Journal::from(InMemoryEventStore::default());
    let order_id = AggregateId::new("saga-upstream-mixed-order");

    append_order_placed(&upstream_store, &order_id, 1, "ORDER-1").await;
    append_unrelated(&upstream_store, &order_id, 2).await;
    append_order_placed(&upstream_store, &order_id, 3, "ORDER-2").await;

    let saga_store = Journal::from(InMemoryEventStore::default());
    let captured: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let notify = Arc::new(Notify::new());

    let captured_for_producer = Arc::clone(&captured);
    let notify_for_producer = Arc::clone(&notify);

    let _saga_proxy =
        SagaManagerProps::<RecordingSaga>::new(saga_store, move |_components| RecordingSaga {
            captured: Arc::clone(&captured_for_producer),
            notify: Arc::clone(&notify_for_producer),
        })
        .observing::<OrderPlaced>(Subscription::stream(&upstream_store, &order_id))
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

/// The store-wide cursor, which is the only one that can order records of
/// several aggregates against each other.
///
/// `Subscription` currently constructs only a stream-keyed position
/// ([`Subscription::stream`]), so the constructor named below does not exist
/// yet: the assertion is what this file is for, and a stream-keyed subscription
/// cannot carry it — a single stream key sees one aggregate.  The internal
/// `SequenceCursor::Global` the saga machinery passes through unchanged is the
/// position this needs a public constructor for.
#[tokio::test]
async fn saga_catchup_with_global_cursor_orders_across_aggregates() {
    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::builder(ps)
        .with_persistence(PersistenceProvider::new(InMemoryStore::default()))
        .build();
    let upstream_store = Journal::from(InMemoryEventStore::default());

    let agg_a = AggregateId::new("saga-upstream-global-a");
    let agg_b = AggregateId::new("saga-upstream-global-b");

    append_order_placed(&upstream_store, &agg_a, 1, "A1").await;
    append_order_placed(&upstream_store, &agg_b, 1, "B1").await;
    append_order_placed(&upstream_store, &agg_a, 2, "A2").await;
    append_order_placed(&upstream_store, &agg_b, 2, "B2").await;

    let saga_store = Journal::from(InMemoryEventStore::default());
    let captured: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let notify = Arc::new(Notify::new());

    let captured_for_producer = Arc::clone(&captured);
    let notify_for_producer = Arc::clone(&notify);

    let _saga_proxy =
        SagaManagerProps::<RecordingSaga>::new(saga_store, move |_components| RecordingSaga {
            captured: Arc::clone(&captured_for_producer),
            notify: Arc::clone(&notify_for_producer),
        })
        .observing::<OrderPlaced>(Subscription::global(&upstream_store))
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
