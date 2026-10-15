//! Hosting a saga through the extension trait on `EventSourceSystem`.
//!
//! The wiring under test is the one the caller no longer writes: both codecs
//! are resolved from the event types themselves (`Event::Codec`), the
//! `ProcessSystem` is never handed over, and store + cursor arrive as a single
//! subscription value.  Each test therefore observes the two codec directions
//! (upstream record decoded into `Saga::handle`, the saga's own event encoded
//! onto its stream) and the cursor position the subscription value carries.
//!
//! `host` is the one system-side verb, and it always starts a manager: the
//! saga below answers the same `SagaId` for every input, so what runs is the
//! degenerate single-instance case.  That `host` really is the whole of the
//! resident-saga wiring is pinned in
//! `saga_manager_degenerate_single_instance.rs`; here it is the entry point's
//! own resolutions that are under test.
//!
//! Run with: `cargo test -p nitinol-saga --test saga_host_entry_point`.

#[path = "common/helpers.rs"]
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

use nitinol_eventsource::{system::EventSourceSystem, Event};
use nitinol_persistence::provider::{CustomStore, PersistenceProvider};
use nitinol_persistence::store::{InMemoryEventStore, InMemoryOffsetStore, InMemoryStore, Journal};
use nitinol_persistence::{
    AggregateId, AppendingEvent, EventType, Family, LoadQuery, LoadedEvent, TypeName,
};
use nitinol_runtime::ProcessSystem;
use nitinol_saga::{
    Saga, SagaContext, SagaEffect, SagaId, SagaSubscribe, SagaSystemExt, Subscription,
};

#[derive(Clone, Debug, Serialize, Deserialize)]
struct OrderPlaced {
    sku: String,
}

impl Event for OrderPlaced {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("saga.host.entry"), TypeName::new("OrderPlaced"));
    type Codec = JsonCodec;
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ReservationRequested {
    sku: String,
}

impl Event for ReservationRequested {
    const EVENT_TYPE: EventType = EventType::new(
        Family::new("saga.host.entry"),
        TypeName::new("ReservationRequested"),
    );
    type Codec = JsonCodec;
}

/// Correlation rule of [`RecordingSaga`]: every order belongs to the single
/// reservation process each test in this file hosts.
const RECORDING_SAGA_ID: &str = "saga-host-entry-reservation";

/// The whole alphabet this saga reacts to: one observed source, no deadline.
#[derive(SagaSubscribe)]
enum Observed {
    Order(OrderPlaced),
}

struct RecordingSaga {
    handled: Arc<Mutex<Vec<String>>>,
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
        self.handled
            .lock()
            .expect("handled mutex is never poisoned: no holder panics while the guard is alive")
            .push(event.sku.clone());
        self.notify.notify_one();
        Ok(SagaEffect::persist(ReservationRequested { sku: event.sku }))
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

fn handled_skus(handled: &Arc<Mutex<Vec<String>>>) -> Vec<String> {
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
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let notified = notify.notified();
            if handled_skus(handled).len() >= expected {
                return;
            }
            notified.await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "timed out waiting for {expected} handle() calls (got {:?})",
            handled_skus(handled)
        )
    });
}

/// SKUs of the saga's own events on its own stream, decoded with the same JSON
/// wire format the codec `ReservationRequested` names produces.
async fn persisted_skus(store: &Journal, saga_id: &SagaId) -> Vec<String> {
    let events: Vec<LoadedEvent> = store
        .load(LoadQuery::by_stream(saga_id))
        .await
        .expect("saga event store load must succeed")
        .try_collect()
        .await
        .expect("collect saga events must succeed");
    events
        .iter()
        .filter(|event| event.event_type == ReservationRequested::EVENT_TYPE)
        .map(|event| {
            serde_json::from_slice::<ReservationRequested>(&event.payload)
                .expect("the saga's own event must be encoded with the codec its event type names")
                .sku
        })
        .collect()
}

async fn wait_for_persisted(store: &Journal, saga_id: &SagaId) -> Vec<String> {
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    loop {
        let skus = persisted_skus(store, saga_id).await;
        if !skus.is_empty() {
            return skus;
        }
        if std::time::Instant::now() >= deadline {
            panic!("timed out waiting for the saga to persist a ReservationRequested event");
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

#[tokio::test]
async fn host_uses_the_codecs_its_events_name() {
    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::builder(ps)
        .with_persistence(PersistenceProvider::new(
            CustomStore::builder()
                .eventstore(InMemoryEventStore::default())
                .saga_offsets(InMemoryOffsetStore::default())
                .build(),
        ))
        .build();
    let journal = system.persistence().event_store();

    let order_store = Journal::from(InMemoryEventStore::default());
    let order_id = AggregateId::new("saga-host-entry-order");

    let saga_id = SagaId::new(RECORDING_SAGA_ID);

    let handled: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let notify = Arc::new(Notify::new());
    let handled_for_producer = Arc::clone(&handled);
    let notify_for_producer = Arc::clone(&notify);

    let _manager = system
        .host::<RecordingSaga>(move |_components| RecordingSaga {
            handled: Arc::clone(&handled_for_producer),
            notify: Arc::clone(&notify_for_producer),
        })
        .observing::<OrderPlaced>(Subscription::stream(&order_store, &order_id))
        .spawn()
        .await;

    // The upstream store stays owned by the caller after the subscription is
    // built, exactly as the example needs it for its own aggregate wiring.
    append_order_placed(&order_store, &order_id, 1, "SKU-EXT").await;

    wait_for_handled(&handled, &notify, 1).await;
    assert_eq!(
        handled_skus(&handled),
        vec!["SKU-EXT".to_owned()],
        "the codec `OrderPlaced` names must decode the subscribed record into handle()"
    );

    assert_eq!(
        wait_for_persisted(&journal, &saga_id).await,
        vec!["SKU-EXT".to_owned()],
        "the codec `ReservationRequested` names must encode exactly one event onto the saga stream"
    );
}

#[tokio::test]
async fn stream_subscription_starts_at_the_beginning_of_the_upstream_stream() {
    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::builder(ps)
        .with_persistence(PersistenceProvider::new(InMemoryStore::default()))
        .build();

    let order_store = Journal::from(InMemoryEventStore::default());
    let order_id = AggregateId::new("saga-host-entry-catchup-order");
    append_order_placed(&order_store, &order_id, 1, "BEFORE-1").await;
    append_order_placed(&order_store, &order_id, 2, "BEFORE-2").await;

    let handled: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let notify = Arc::new(Notify::new());
    let handled_for_producer = Arc::clone(&handled);
    let notify_for_producer = Arc::clone(&notify);

    let _manager = system
        .host::<RecordingSaga>(move |_components| RecordingSaga {
            handled: Arc::clone(&handled_for_producer),
            notify: Arc::clone(&notify_for_producer),
        })
        .observing::<OrderPlaced>(Subscription::stream(&order_store, &order_id))
        .spawn()
        .await;

    wait_for_handled(&handled, &notify, 2).await;
    assert_eq!(
        handled_skus(&handled),
        vec!["BEFORE-1".to_owned(), "BEFORE-2".to_owned()],
        "the default stream position must deliver records that pre-date the spawn, in order"
    );
}

#[tokio::test]
async fn stream_subscription_after_override_skips_records_up_to_that_position() {
    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::builder(ps)
        .with_persistence(PersistenceProvider::new(InMemoryStore::default()))
        .build();

    let order_store = Journal::from(InMemoryEventStore::default());
    let order_id = AggregateId::new("saga-host-entry-resume-order");
    append_order_placed(&order_store, &order_id, 1, "SKIPPED").await;
    append_order_placed(&order_store, &order_id, 2, "DELIVERED").await;

    let handled: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let notify = Arc::new(Notify::new());
    let handled_for_producer = Arc::clone(&handled);
    let notify_for_producer = Arc::clone(&notify);

    let _manager = system
        .host::<RecordingSaga>(move |_components| RecordingSaga {
            handled: Arc::clone(&handled_for_producer),
            notify: Arc::clone(&notify_for_producer),
        })
        .observing::<OrderPlaced>(Subscription::stream(&order_store, &order_id).with_after(1))
        .spawn()
        .await;

    wait_for_handled(&handled, &notify, 1).await;
    // Catchup delivers in ascending sequence order, so a delivered record 1
    // would already be visible here; the settle window matches the convention
    // of the other upstream-delivery tests for records that must never arrive.
    tokio::time::sleep(Duration::from_millis(300)).await;

    assert_eq!(
        handled_skus(&handled),
        vec!["DELIVERED".to_owned()],
        "an overridden position must skip records at or below it and deliver only later ones"
    );
}
