//! Cursor semantics of the manager's single shared subscription.
//!
//! Collapsing `N` per-instance subscriptions into one moves "do not advance the
//! cursor on failure" from an instance decision to a manager decision, and makes
//! the opposite mistake newly possible: one instance holding the shared cursor
//! stalls delivery for every other instance.  The three tests below pin both
//! directions.

#[path = "common/helpers.rs"]
mod common;
use common::{encode_outbox_ended, JsonCodec, OUTBOX_MARKER};

use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use futures_core::Stream;
use futures_util::TryStreamExt;
use serde::{Deserialize, Serialize};
use tokio::sync::Notify;

use nitinol_eventsource::{system::EventSourceSystem, Event, SequenceCursor, SystemEvent};
use nitinol_persistence::error::{AppendError, LoadError};
use nitinol_persistence::store::{EventStore, EventStream, InMemoryEventStore};
use nitinol_persistence::{
    AggregateId, AppendOutcome, AppendingEvent, EventType, Family, LoadQuery, LoadedEvent, TypeName,
};
use nitinol_runtime::ProcessSystem;
use nitinol_saga::{
    DeadLetterEvent, Saga, SagaContext, SagaEffect, SagaFailure, SagaId, SagaManagerProps,
};

/// Instance-id prefix used by [`HoldSaga`].
const HOLD_PREFIX: &str = "mgr-hold-";
/// Instance-id prefix used by [`SelectiveSaga`].
const SELECTIVE_PREFIX: &str = "mgr-selective-";
/// Instance-id prefix used by [`FanOutSaga`].
const FANOUT_PREFIX: &str = "mgr-fanout-";

// Domain types

#[derive(Clone, Debug, Serialize, Deserialize)]
struct OrderPlaced {
    order: String,
}

impl Event for OrderPlaced {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("mgr.cursor"), TypeName::new("OrderPlaced"));
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ReservationRequested {
    order: String,
}

impl Event for ReservationRequested {
    const EVENT_TYPE: EventType = EventType::new(
        Family::new("mgr.cursor"),
        TypeName::new("ReservationRequested"),
    );
}

#[derive(Debug)]
struct HandleBoom;

impl std::fmt::Display for HandleBoom {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "handle refused the event")
    }
}

impl std::error::Error for HandleBoom {}

// Sagas.  Correlation belongs to the saga type, so each rule needs its own type.

/// Always rejects the event.  Paired with a store whose appends fail, so the
/// rejection cannot even be recorded as a dead letter and the instance reports
/// the message as unprocessed.
struct HoldSaga {
    seen: Arc<Mutex<Vec<String>>>,
    notify: Arc<Notify>,
}

#[async_trait]
impl Saga for HoldSaga {
    type SubscribedEvent = OrderPlaced;
    type Event = ReservationRequested;
    type ScheduledMessage = ();
    type Error = HandleBoom;

    fn correlate(event: &Self::SubscribedEvent) -> Option<SagaId> {
        Some(SagaId::new(format!("{HOLD_PREFIX}{}", event.order)))
    }

    fn apply(&mut self, _event: Self::Event) {}

    async fn handle(
        &mut self,
        event: Self::SubscribedEvent,
        _ctx: &mut SagaContext,
    ) -> Result<SagaEffect<Self::Event>, Self::Error> {
        record(&self.seen, &self.notify, event.order);
        Err(HandleBoom)
    }
}

/// Claims only the orders carrying the `MATCH-` prefix; everything else
/// correlates to no instance at all.
struct SelectiveSaga {
    seen: Arc<Mutex<Vec<String>>>,
    notify: Arc<Notify>,
}

#[async_trait]
impl Saga for SelectiveSaga {
    type SubscribedEvent = OrderPlaced;
    type Event = ReservationRequested;
    type ScheduledMessage = ();
    type Error = std::convert::Infallible;

    fn correlate(event: &Self::SubscribedEvent) -> Option<SagaId> {
        event
            .order
            .starts_with("MATCH-")
            .then(|| SagaId::new(format!("{SELECTIVE_PREFIX}{}", event.order)))
    }

    fn apply(&mut self, _event: Self::Event) {}

    async fn handle(
        &mut self,
        event: Self::SubscribedEvent,
        _ctx: &mut SagaContext,
    ) -> Result<SagaEffect<Self::Event>, Self::Error> {
        record(&self.seen, &self.notify, event.order);
        Ok(SagaEffect::None)
    }
}

/// One instance per order key — used to place an already-ended instance and a
/// live one behind the same shared subscription.
struct FanOutSaga {
    seen: Arc<Mutex<Vec<String>>>,
    notify: Arc<Notify>,
}

#[async_trait]
impl Saga for FanOutSaga {
    type SubscribedEvent = OrderPlaced;
    type Event = ReservationRequested;
    type ScheduledMessage = ();
    type Error = std::convert::Infallible;

    fn correlate(event: &Self::SubscribedEvent) -> Option<SagaId> {
        Some(SagaId::new(format!("{FANOUT_PREFIX}{}", event.order)))
    }

    fn apply(&mut self, _event: Self::Event) {}

    async fn handle(
        &mut self,
        event: Self::SubscribedEvent,
        _ctx: &mut SagaContext,
    ) -> Result<SagaEffect<Self::Event>, Self::Error> {
        record(&self.seen, &self.notify, event.order);
        Ok(SagaEffect::None)
    }
}

/// An `EventStore` that rejects every append and reads back as empty.  Mirrors
/// the shape of the crate-internal `AlwaysFailStore` used by the dead-letter
/// unit tests: replay succeeds (nothing to replay) but no failure can ever be
/// recorded durably.
struct AlwaysFailAppendStore;

#[async_trait]
impl EventStore for AlwaysFailAppendStore {
    async fn append(
        &self,
        _key: &str,
        _events: Vec<AppendingEvent>,
    ) -> Result<AppendOutcome, AppendError> {
        Err(AppendError::Backend("injected: every append fails".into()))
    }

    async fn load(&self, _query: LoadQuery) -> Result<EventStream<'_>, LoadError> {
        let stream: Pin<Box<dyn Stream<Item = Result<LoadedEvent, LoadError>> + Send + '_>> =
            Box::pin(futures_util::stream::empty());
        Ok(stream)
    }
}

// Helpers

fn record(seen: &Arc<Mutex<Vec<String>>>, notify: &Arc<Notify>, order: String) {
    seen.lock()
        .expect("seen mutex is never poisoned: no holder panics while the guard is alive")
        .push(order);
    notify.notify_one();
}

fn seen_snapshot(seen: &Arc<Mutex<Vec<String>>>) -> Vec<String> {
    seen.lock()
        .expect("seen mutex is never poisoned: no holder panics while the guard is alive")
        .clone()
}

async fn wait_for_seen(seen: &Arc<Mutex<Vec<String>>>, notify: &Arc<Notify>, expected: usize) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let notified = notify.notified();
            if seen_snapshot(seen).len() >= expected {
                return;
            }
            notified.await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "timed out waiting for {expected} handle() calls (got {:?})",
            seen_snapshot(seen)
        )
    });
}

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

/// Seed a durable `Ended` outbox marker so the instance replays as terminated.
async fn seed_ended_marker(store: &Arc<dyn EventStore>, saga_id: &SagaId, sequence: u64) {
    store
        .append(
            saga_id.as_str(),
            vec![AppendingEvent {
                sequence,
                event_type: OUTBOX_MARKER,
                payload: encode_outbox_ended(),
                occurred_at: jiff::Timestamp::now(),
            }],
        )
        .await
        .expect("seed Ended marker must succeed");
}

async fn load_stream(store: &Arc<dyn EventStore>, saga_id: &SagaId) -> Vec<LoadedEvent> {
    store
        .load(LoadQuery::by_stream(saga_id))
        .await
        .expect("load saga stream must succeed")
        .try_collect()
        .await
        .expect("collect saga events must succeed")
}

fn count_ended_saga_dead_letters(events: &[LoadedEvent]) -> usize {
    events
        .iter()
        .filter(|e| {
            e.event_type.type_key() == <DeadLetterEvent as SystemEvent>::EVENT_TYPE.type_key()
                && matches!(
                    <DeadLetterEvent as SystemEvent>::decode(&e.payload),
                    Ok(DeadLetterEvent {
                        failure: SagaFailure::EndedSagaReceivedMessage { .. },
                        ..
                    })
                )
        })
        .count()
}

// Tests

/// The instance cannot settle the message durably (it
/// rejects the event and the dead-letter append fails too), so the manager must
/// not advance the shared cursor — the poller redelivers the same record.
#[tokio::test]
async fn instance_handler_error_holds_the_shared_cursor() {
    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::new(ps).with_codec::<JsonCodec>().build();

    let upstream_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let upstream_key = AggregateId::new("mgr-hold-orders");
    append_order_placed(&upstream_store, &upstream_key, 1, "HOLD-1").await;

    let saga_store: Arc<dyn EventStore> = Arc::new(AlwaysFailAppendStore);

    let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let notify = Arc::new(Notify::new());

    let seen_for_producer = Arc::clone(&seen);
    let notify_for_producer = Arc::clone(&notify);

    let _manager_proxy =
        SagaManagerProps::<HoldSaga>::new(Arc::clone(&saga_store), move || HoldSaga {
            seen: Arc::clone(&seen_for_producer),
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

    wait_for_seen(&seen, &notify, 2).await;

    let delivered = seen_snapshot(&seen);
    assert!(
        delivered.len() >= 2,
        "an upstream record the instance could not settle must be redelivered; \
         got {delivered:?}"
    );
    assert!(
        delivered.iter().all(|order| order == "HOLD-1"),
        "the redelivered record must be the one that failed — the cursor must \
         hold at it rather than advancing past it; got {delivered:?}"
    );
}

/// An event that correlates to no instance belongs to
/// nobody, so the manager must advance past it.  Holding it would starve every
/// later record on the shared subscription.
#[tokio::test]
async fn uncorrelated_event_advances_the_cursor() {
    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::new(ps).with_codec::<JsonCodec>().build();

    let upstream_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let upstream_key = AggregateId::new("mgr-selective-orders");
    append_order_placed(&upstream_store, &upstream_key, 1, "SKIP-1").await;
    append_order_placed(&upstream_store, &upstream_key, 2, "MATCH-1").await;

    let saga_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());

    let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let notify = Arc::new(Notify::new());

    let seen_for_producer = Arc::clone(&seen);
    let notify_for_producer = Arc::clone(&notify);

    let _manager_proxy =
        SagaManagerProps::<SelectiveSaga>::new(Arc::clone(&saga_store), move || SelectiveSaga {
            seen: Arc::clone(&seen_for_producer),
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

    // Reaching MATCH-1 at all proves SKIP-1 did not hold the cursor.
    wait_for_seen(&seen, &notify, 1).await;
    tokio::time::sleep(Duration::from_millis(400)).await;

    assert_eq!(
        seen_snapshot(&seen),
        vec!["MATCH-1".to_owned()],
        "the record correlating to no instance must be skipped, not held, and it \
         must never reach handle()"
    );
}

/// A record addressed to an instance that already ended must
/// leave a durable trace on that instance's stream and still let the shared
/// subscription move on to the next instance's record.
#[tokio::test]
async fn ended_instance_does_not_stall_the_shared_subscription() {
    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::new(ps).with_codec::<JsonCodec>().build();

    let upstream_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let upstream_key = AggregateId::new("mgr-fanout-orders");
    append_order_placed(&upstream_store, &upstream_key, 1, "ended").await;
    append_order_placed(&upstream_store, &upstream_key, 2, "live").await;

    let saga_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let ended_instance = SagaId::new(format!("{FANOUT_PREFIX}ended"));
    seed_ended_marker(&saga_store, &ended_instance, 1).await;

    let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let notify = Arc::new(Notify::new());

    let seen_for_producer = Arc::clone(&seen);
    let notify_for_producer = Arc::clone(&notify);

    let _manager_proxy =
        SagaManagerProps::<FanOutSaga>::new(Arc::clone(&saga_store), move || FanOutSaga {
            seen: Arc::clone(&seen_for_producer),
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

    wait_for_seen(&seen, &notify, 1).await;
    tokio::time::sleep(Duration::from_millis(400)).await;

    assert_eq!(
        seen_snapshot(&seen),
        vec!["live".to_owned()],
        "the record for the ended instance must not reach handle(), and it must \
         not stop the record behind it from reaching the live instance"
    );

    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    let ended_dead_letters = loop {
        let count = count_ended_saga_dead_letters(&load_stream(&saga_store, &ended_instance).await);
        if count >= 1 {
            break count;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the record addressed to the ended instance must be recorded as a \
             SagaFailure::EndedSagaReceivedMessage on that instance's own stream, \
             not dropped without a trace"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    };
    assert_eq!(
        ended_dead_letters, 1,
        "exactly one dead letter must be recorded for the single record the \
         ended instance received"
    );
}
