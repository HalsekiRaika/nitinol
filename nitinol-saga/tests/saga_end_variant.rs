//! The `End` variant is the single-responsibility termination
//! marker.  Interpreting it must stop the saga process (`ctx.stop_self()`),
//! which causes the subscriber to stop via the direct poller's subscriber
//! watch, and in turn tears down the upstream subscription.
//!
//! Also exercises the Sequence short-circuit invariant: effects placed after
//! `End` inside a Sequence are not interpreted (the interpreter must break).

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

use nitinol_eventsource::{system::EventSourceSystem, Event, SequenceCursor};
use nitinol_persistence::store::{EventStore, InMemoryEventStore};
use nitinol_persistence::{
    AggregateId, AppendingEvent, EventType, Family, LoadQuery, LoadedEvent, TypeName,
};
use nitinol_runtime::ProcessSystem;
use nitinol_saga::{Saga, SagaContext, SagaEffect, SagaId, SagaProps};

#[derive(Clone, Debug, Serialize, Deserialize)]
struct OrderPlaced {
    sku: String,
}

impl Event for OrderPlaced {
    const EVENT_TYPE: EventType = EventType::new(Family::new("end"), TypeName::new("OrderPlaced"));
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ReservationRequested {
    sku: String,
}

impl Event for ReservationRequested {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("end"), TypeName::new("ReservationRequested"));
}

/// Correlation rule of [`PersistThenEndSaga`]: this file drives one order
/// process, so every `OrderPlaced` names that single instance.
const PERSIST_THEN_END_SAGA_ID: &str = "end-saga-1";

/// A saga that emits `Persist.combine(End)` so we can verify both:
/// 1. The user event is persisted before End is interpreted
/// 2. Effects after End in the same handle call must not be interpreted
struct PersistThenEndSaga {
    handle_count: Arc<Mutex<u64>>,
    handled_first: Arc<Notify>,
}

#[async_trait]
impl Saga for PersistThenEndSaga {
    type SubscribedEvent = OrderPlaced;
    type Event = ReservationRequested;
    type ScheduledMessage = ();
    type Error = std::convert::Infallible;

    fn correlate(_event: &Self::SubscribedEvent) -> Option<SagaId> {
        Some(SagaId::new(PERSIST_THEN_END_SAGA_ID))
    }

    fn apply(&mut self, _event: Self::Event) {}

    async fn handle(
        &mut self,
        event: Self::SubscribedEvent,
        _ctx: &mut SagaContext,
    ) -> Result<SagaEffect<Self::Event>, Self::Error> {
        *self.handle_count.lock().expect(
            "handle_count mutex is never poisoned: no holder panics while the guard is alive",
        ) += 1;

        // After-End effect is a Persist that would be observable in the stream
        // (a second user event at sequence 2) if the Sequence short-circuit
        // didn't fire.  Its absence is the test signal.
        let effect = SagaEffect::persist(ReservationRequested {
            sku: event.sku.clone(),
        })
        .then_end()
        .combine(SagaEffect::persist(ReservationRequested {
            sku: format!("{}-AFTER-END", event.sku),
        }));

        self.handled_first.notify_one();
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

#[tokio::test]
async fn end_variant_stops_saga_process_and_prevents_further_handle_calls() {
    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::builder(ps)
        .with_codec::<JsonCodec>()
        .build();

    let upstream_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let order_id = AggregateId::new("end-order");
    append_order_placed(&upstream_store, &order_id, 1, "FIRST").await;

    let saga_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let saga_id = SagaId::new(PERSIST_THEN_END_SAGA_ID);
    let handle_count: Arc<Mutex<u64>> = Arc::new(Mutex::new(0));
    let handled_first = Arc::new(Notify::new());

    let handle_count_clone = Arc::clone(&handle_count);
    let handled_first_clone = Arc::clone(&handled_first);

    let _saga_proxy =
        SagaProps::<PersistThenEndSaga>::new(saga_id.clone(), Arc::clone(&saga_store), move || {
            PersistThenEndSaga {
                handle_count: Arc::clone(&handle_count_clone),
                handled_first: Arc::clone(&handled_first_clone),
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

    tokio::time::timeout(Duration::from_secs(3), handled_first.notified())
        .await
        .expect("Saga::handle must run for the first event within 3 seconds");

    // Wait briefly for the End interpretation to actually stop the process and
    // for any "after-End" Persist to (incorrectly) land if the short-circuit
    // is broken.
    tokio::time::sleep(Duration::from_millis(200)).await;

    let events_after_first = load_saga_events(&saga_store, &saga_id).await;
    let user_events_after_first: Vec<&LoadedEvent> = events_after_first
        .iter()
        .filter(|e| e.event_type == ReservationRequested::EVENT_TYPE)
        .collect();
    assert_eq!(
        user_events_after_first.len(),
        1,
        "Persist.then_end().combine(Persist) — the second Persist must NOT be interpreted \
         because End short-circuits the Sequence; got {} user events",
        user_events_after_first.len()
    );

    // Now push a second upstream event.  If End correctly stopped the saga
    // process and its DurableStream subscription, handle() will not run again
    // and no further user events will be persisted.
    append_order_placed(&upstream_store, &order_id, 2, "SECOND").await;
    tokio::time::sleep(Duration::from_millis(500)).await;

    let final_handle_count = *handle_count
        .lock()
        .expect("handle_count mutex is never poisoned: no holder panics while the guard is alive");
    assert_eq!(
        final_handle_count, 1,
        "after End, the saga process must be stopped; \
         Saga::handle must not be invoked for any subsequent upstream event"
    );

    let final_events = load_saga_events(&saga_store, &saga_id).await;
    let final_user_events: Vec<&LoadedEvent> = final_events
        .iter()
        .filter(|e| e.event_type == ReservationRequested::EVENT_TYPE)
        .collect();
    assert_eq!(
        final_user_events.len(),
        1,
        "the saga stream must contain exactly one user event after End — \
         no second event must appear for the second upstream OrderPlaced"
    );
}
