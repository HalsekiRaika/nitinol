//! Single-instance delivery is decided by [`Saga::correlate`].  (Contracts
//! `CT-03`, `CT-02`, `CT-09`.)
//!
//! The runtime consults the saga *type* for every decoded upstream event and
//! compares the answer with the spawned instance's own [`SagaId`]:
//!
//! | `correlate(&event)`         | outcome                        |
//! |-----------------------------|--------------------------------|
//! | `None`                      | ignored                        |
//! | `Some(id)`, `id != self`    | ignored                        |
//! | `Some(id)`, `id == self`    | delivered to [`Saga::handle`]  |
//!
//! All three classes travel through **one** upstream stream in a single test so
//! that an implementation satisfying only part of the table is identified:
//! treating `None` as "mine" admits the `NONE-*` events, and dropping the
//! self-comparison admits the `OTHER-*` events.
//!
//! "Ignored" is a *silent* outcome, not a dead letter — pinned separately below
//! so that an implementation which dead-letters every event it declines is
//! rejected rather than passing on the delivery assertion alone.
//!
//! `CT-02`: every spawn here calls `with_subscription` with three arguments
//! (`upstream_store`, `upstream_codec`, `cursor`).  A builder that still takes
//! a routing argument — positionally or as a trailing `Option` — fails to
//! compile against these call sites.

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

use nitinol_eventsource::system::EventSourceSystem;
use nitinol_eventsource::{Event, SequenceCursor};
use nitinol_persistence::store::{EventStore, InMemoryEventStore};
use nitinol_persistence::{
    AggregateId, AppendingEvent, EventType, Family, LoadQuery, LoadedEvent, TypeName,
};
use nitinol_runtime::ProcessSystem;
use nitinol_saga::{Saga, SagaContext, SagaEffect, SagaId, SagaProps};

// Correlation rule — one definition point, shared by `correlate` and by the
// spawn sites below.  The spawned instance is `OWN_SAGA_ID`; `FOREIGN_SAGA_ID`
// is never spawned, so events correlating to it belong to nobody present.

const OWN_SAGA_ID: &str = "correlate-routing-own";
const FOREIGN_SAGA_ID: &str = "correlate-routing-foreign";

/// Type-level identity of a dead-letter event, as observed on the wire.
const DEAD_LETTER_TYPE: EventType =
    EventType::new(Family::new("nitinol.saga"), TypeName::new("dead_letter"));

#[derive(Clone, Debug, Serialize, Deserialize)]
struct OrderPlaced {
    sku: String,
}

impl Event for OrderPlaced {
    const EVENT_TYPE: EventType = EventType::new(
        Family::new("saga.correlate.routing"),
        TypeName::new("OrderPlaced"),
    );
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ReservationRequested {
    sku: String,
}

impl Event for ReservationRequested {
    const EVENT_TYPE: EventType = EventType::new(
        Family::new("saga.correlate.routing"),
        TypeName::new("ReservationRequested"),
    );
}

/// Records every event that actually reaches `handle`.
struct CorrelationSaga {
    captured: Arc<Mutex<Vec<String>>>,
    notify: Arc<Notify>,
}

#[async_trait]
impl Saga for CorrelationSaga {
    type SubscribedEvent = OrderPlaced;
    type Event = ReservationRequested;
    type ScheduledMessage = ();
    type Error = std::convert::Infallible;

    /// Derives the owning instance from the SKU prefix so that one upstream
    /// stream can carry all three correlation outcomes.
    fn correlate(event: &Self::SubscribedEvent) -> Option<SagaId> {
        if event.sku.starts_with("MINE-") {
            Some(SagaId::new(OWN_SAGA_ID))
        } else if event.sku.starts_with("OTHER-") {
            Some(SagaId::new(FOREIGN_SAGA_ID))
        } else {
            None
        }
    }

    fn apply(&mut self, _event: Self::Event) {}

    async fn handle(
        &mut self,
        event: Self::SubscribedEvent,
        _ctx: &mut SagaContext,
    ) -> Result<SagaEffect<Self::Event>, Self::Error> {
        self.captured
            .lock()
            .expect("captured mutex is never poisoned: no holder panics while the guard is alive")
            .push(event.sku.clone());
        self.notify.notify_one();
        Ok(SagaEffect::None)
    }
}

// Helpers

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

/// The upstream stream every test in this file uses: the three correlation
/// classes interleaved, with a declined event *last* so that a run which
/// wrongly accepts it is still observed after the wait for the matching ones.
async fn append_mixed_correlation_stream(store: &Arc<dyn EventStore>, agg_id: &AggregateId) {
    for (sequence, sku) in [
        (1u64, "NONE-1"),
        (2, "OTHER-1"),
        (3, "MINE-1"),
        (4, "NONE-2"),
        (5, "MINE-2"),
        (6, "OTHER-2"),
        (7, "NONE-3"),
    ] {
        append_order_placed(store, agg_id, sequence, sku).await;
    }
}

fn captured_snapshot(captured: &Arc<Mutex<Vec<String>>>) -> Vec<String> {
    captured
        .lock()
        .expect("captured mutex is never poisoned: no holder panics while the guard is alive")
        .clone()
}

async fn wait_for_count(captured: &Arc<Mutex<Vec<String>>>, notify: &Arc<Notify>, expected: usize) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let notified = notify.notified();
            if captured_snapshot(captured).len() >= expected {
                return;
            }
            notified.await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "timed out waiting for {expected} handle() calls (got {:?}) — the \
             saga's own correlate result must match the SagaId it was spawned \
             with, otherwise nothing is ever delivered",
            captured_snapshot(captured)
        )
    });
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

/// Spawn the saga under test over `upstream_store`, subscribing to `agg_id`
/// from the beginning.  No routing argument is passed — correlation lives on
/// `CorrelationSaga`.
async fn spawn_correlation_saga(
    system: &EventSourceSystem<JsonCodec>,
    upstream_store: &Arc<dyn EventStore>,
    agg_id: &AggregateId,
    saga_store: Arc<dyn EventStore>,
    captured: &Arc<Mutex<Vec<String>>>,
    notify: &Arc<Notify>,
) {
    let captured_for_producer = Arc::clone(captured);
    let notify_for_producer = Arc::clone(notify);

    let _saga_proxy =
        SagaProps::<CorrelationSaga>::new(SagaId::new(OWN_SAGA_ID), saga_store, move || {
            CorrelationSaga {
                captured: Arc::clone(&captured_for_producer),
                notify: Arc::clone(&notify_for_producer),
            }
        })
        .with_codec(system.codec::<ReservationRequested>())
        .with_subscription(
            Arc::clone(upstream_store),
            system.codec::<OrderPlaced>(),
            SequenceCursor::Stream {
                key: agg_id.as_str().to_owned(),
                after: 0,
            },
        )
        .spawn(system.process_system())
        .await;
}

// Tests

/// Given one upstream stream carrying events that correlate to nobody, to a
/// different instance, and to this instance,
/// when the saga subscribes to it,
/// then only the events correlating to this instance's own `SagaId` reach
/// `Saga::handle`, in upstream order.
#[tokio::test]
async fn only_events_correlating_to_this_saga_reach_handle() {
    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::new(ps).with_codec::<JsonCodec>().build();

    let upstream_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let order_id = AggregateId::new("correlate-routing-order");
    append_mixed_correlation_stream(&upstream_store, &order_id).await;

    let saga_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let captured: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let notify = Arc::new(Notify::new());

    spawn_correlation_saga(
        &system,
        &upstream_store,
        &order_id,
        saga_store,
        &captured,
        &notify,
    )
    .await;

    wait_for_count(&captured, &notify, 2).await;
    // The stream ends with two declined events; give them time to be delivered
    // and dropped so a run that wrongly accepts them is observed here.
    tokio::time::sleep(Duration::from_millis(300)).await;

    assert_eq!(
        captured_snapshot(&captured),
        vec!["MINE-1".to_owned(), "MINE-2".to_owned()],
        "`handle` must see exactly the events whose correlate result equals \
         this instance's SagaId: `NONE-*` correlate to nobody and `OTHER-*` \
         correlate to an instance that is not this one"
    );
}

/// Given the same stream,
/// when the saga declines events by correlation,
/// then declining is silent — no dead letter is written to the saga's own
/// stream for the events it did not accept.
#[tokio::test]
async fn correlation_mismatch_is_silent_and_writes_no_dead_letter() {
    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::new(ps).with_codec::<JsonCodec>().build();

    let upstream_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let order_id = AggregateId::new("correlate-routing-silent-order");
    append_mixed_correlation_stream(&upstream_store, &order_id).await;

    let saga_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let captured: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let notify = Arc::new(Notify::new());

    spawn_correlation_saga(
        &system,
        &upstream_store,
        &order_id,
        Arc::clone(&saga_store),
        &captured,
        &notify,
    )
    .await;

    // Reaching two accepted events proves the five declined ones were fetched
    // and evaluated, so the DLQ assertion below is about work that happened.
    wait_for_count(&captured, &notify, 2).await;
    tokio::time::sleep(Duration::from_millis(300)).await;

    let events = load_saga_events(&saga_store, &SagaId::new(OWN_SAGA_ID)).await;
    let dead_letters = events
        .iter()
        .filter(|e| e.event_type.type_key() == DEAD_LETTER_TYPE.type_key())
        .map(|e| e.event_type.to_string())
        .collect::<Vec<_>>();

    assert!(
        dead_letters.is_empty(),
        "an event this saga does not correlate to is not a failure — it belongs \
         to somebody else.  Recording it as a dead letter would make every \
         subscriber of a shared upstream accumulate one entry per foreign \
         event; found: {dead_letters:?}"
    );
}
