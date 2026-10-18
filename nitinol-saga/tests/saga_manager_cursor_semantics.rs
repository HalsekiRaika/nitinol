//! Cursor semantics of the manager's single shared subscription.
//!
//! Collapsing `N` per-instance subscriptions into one moves "do not advance the
//! cursor on failure" from an instance decision to a manager decision, and makes
//! the opposite mistake newly possible: one instance holding the shared cursor
//! stalls delivery for every other instance.  The three tests below pin both
//! directions.

#[path = "common/helpers.rs"]
mod common;
use common::{encode_lifecycle_ended, JsonCodec, LIFECYCLE_MARKER};

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use futures_util::TryStreamExt;
use serde::{Deserialize, Serialize};
use tokio::sync::Notify;

use nitinol_eventsource::{system::EventSourceSystem, Event};
use nitinol_persistence::error::{AppendError, LoadError};
use nitinol_persistence::provider::{CustomStore, PersistenceProvider};
use nitinol_persistence::store::{
    EventStore, EventStream, InMemoryEventStore, InMemoryOffsetStore, InMemoryStore, Journal,
};
use nitinol_persistence::{
    AggregateId, AppendOutcome, AppendingEvent, EventType, Family, LoadQuery, LoadedEvent, TypeName,
};
use nitinol_runtime::ProcessSystem;
use nitinol_saga::{
    Saga, SagaContext, SagaEffect, SagaId, SagaManagerProps, SagaSubscribe, Subscription,
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
    type Codec = JsonCodec;
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
    type Codec = JsonCodec;
}

#[derive(Debug)]
struct HandleBoom;

impl std::fmt::Display for HandleBoom {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "handle refused the event")
    }
}

impl std::error::Error for HandleBoom {}

/// The whole alphabet every saga in this file reacts to: one observed source, no
/// deadline.
#[derive(SagaSubscribe)]
enum Observed {
    Order(OrderPlaced),
}

// Sagas.  Correlation belongs to the saga type, so each rule needs its own type.

/// Always rejects the event.  Paired with a store whose appends fail, so the
/// refused input cannot even be settled by its position and the instance
/// reports the message as unprocessed.
struct HoldSaga {
    seen: Arc<Mutex<Vec<String>>>,
    notify: Arc<Notify>,
}

#[async_trait]
impl Saga for HoldSaga {
    type Subscribe = Observed;
    type Event = ReservationRequested;
    type Error = HandleBoom;

    fn correlate(input: &Self::Subscribe) -> Option<SagaId> {
        match input {
            Observed::Order(event) => Some(SagaId::new(format!("{HOLD_PREFIX}{}", event.order))),
        }
    }

    async fn handle(
        &mut self,
        input: Self::Subscribe,
        _ctx: &mut SagaContext,
    ) -> Result<SagaEffect<Self::Event>, Self::Error> {
        let Observed::Order(event) = input;
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
    type Subscribe = Observed;
    type Event = ReservationRequested;
    type Error = std::convert::Infallible;

    fn correlate(input: &Self::Subscribe) -> Option<SagaId> {
        match input {
            Observed::Order(event) => event
                .order
                .starts_with("MATCH-")
                .then(|| SagaId::new(format!("{SELECTIVE_PREFIX}{}", event.order))),
        }
    }

    async fn handle(
        &mut self,
        input: Self::Subscribe,
        _ctx: &mut SagaContext,
    ) -> Result<SagaEffect<Self::Event>, Self::Error> {
        let Observed::Order(event) = input;
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
    type Subscribe = Observed;
    type Event = ReservationRequested;
    type Error = std::convert::Infallible;

    fn correlate(input: &Self::Subscribe) -> Option<SagaId> {
        match input {
            Observed::Order(event) => Some(SagaId::new(format!("{FANOUT_PREFIX}{}", event.order))),
        }
    }

    async fn handle(
        &mut self,
        input: Self::Subscribe,
        _ctx: &mut SagaContext,
    ) -> Result<SagaEffect<Self::Event>, Self::Error> {
        let Observed::Order(event) = input;
        record(&self.seen, &self.notify, event.order);
        Ok(SagaEffect::None)
    }
}

/// An `EventStore` that rejects every append addressed to one named stream and
/// passes everything else to the in-memory store it wraps.
///
/// A system distributes one journal, so the instance's own stream and the
/// upstream stream it reacts to are tenants of the same store: refusing one key
/// leaves replay succeeding (nothing to replay) while nothing an input produces
/// — not even its consumed position — can ever be recorded durably, and the
/// upstream rows the test seeds stay writable and readable.
#[derive(Clone)]
struct AppendRefusingStore {
    inner: InMemoryEventStore,
    /// The stream whose appends are refused.
    refused: String,
}

impl AppendRefusingStore {
    fn refusing(refused: &str) -> Self {
        Self {
            inner: InMemoryEventStore::default(),
            refused: refused.to_owned(),
        }
    }
}

#[async_trait]
impl EventStore for AppendRefusingStore {
    async fn append(
        &self,
        key: &str,
        events: Vec<AppendingEvent>,
    ) -> Result<AppendOutcome, AppendError> {
        if key == self.refused {
            return Err(AppendError::Backend("injected: every append fails".into()));
        }
        self.inner.append(key, events).await
    }

    async fn load(&self, query: LoadQuery) -> Result<EventStream<'_>, LoadError> {
        self.inner.load(query).await
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
    store: &Journal,
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

/// Seed a durable `Ended` lifecycle fact so the instance replays as terminated.
async fn seed_ended_fact(store: &Journal, saga_id: &SagaId, sequence: u64) {
    store
        .append(
            saga_id.as_str(),
            vec![AppendingEvent {
                sequence,
                event_type: LIFECYCLE_MARKER,
                payload: encode_lifecycle_ended(),
                occurred_at: jiff::Timestamp::now(),
            }],
        )
        .await
        .expect("seed Ended fact must succeed");
}

async fn load_stream(store: &Journal, saga_id: &SagaId) -> Vec<LoadedEvent> {
    store
        .load(LoadQuery::by_stream(saga_id))
        .await
        .expect("load saga stream must succeed")
        .try_collect()
        .await
        .expect("collect saga events must succeed")
}

fn describe(events: &[LoadedEvent]) -> Vec<String> {
    events
        .iter()
        .map(|e| format!("{}@{}", e.event_type, e.sequence))
        .collect()
}

// Tests

/// The instance cannot settle the message durably (it rejects the event and
/// the position append that would settle the refusal fails too), so the manager
/// must not advance the shared cursor — the poller redelivers the same record.
#[tokio::test]
async fn instance_handler_error_holds_the_shared_cursor() {
    // The one instance this upstream record correlates to is the stream whose
    // appends the journal refuses; every other key — the upstream stream
    // included — is writable.
    let held_instance = SagaId::new(format!("{HOLD_PREFIX}HOLD-1"));
    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::builder(ps)
        .with_persistence(PersistenceProvider::new(
            CustomStore::builder()
                .eventstore(AppendRefusingStore::refusing(held_instance.as_str()))
                .saga_offsets(InMemoryOffsetStore::default())
                .build(),
        ))
        .build();
    let journal = system.persistence().event_store();

    let upstream_key = AggregateId::new("mgr-hold-orders");
    append_order_placed(&journal, &upstream_key, 1, "HOLD-1").await;

    let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let notify = Arc::new(Notify::new());

    let seen_for_producer = Arc::clone(&seen);
    let notify_for_producer = Arc::clone(&notify);

    let _manager_proxy = SagaManagerProps::<HoldSaga>::new(move |_components| HoldSaga {
        seen: Arc::clone(&seen_for_producer),
        notify: Arc::clone(&notify_for_producer),
    })
    .observing::<OrderPlaced>(Subscription::stream(&upstream_key))
    .spawn(&system)
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
    let system = EventSourceSystem::builder(ps)
        .with_persistence(PersistenceProvider::new(InMemoryStore::default()))
        .build();

    let journal = system.persistence().event_store();
    let upstream_key = AggregateId::new("mgr-selective-orders");
    append_order_placed(&journal, &upstream_key, 1, "SKIP-1").await;
    append_order_placed(&journal, &upstream_key, 2, "MATCH-1").await;

    let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let notify = Arc::new(Notify::new());

    let seen_for_producer = Arc::clone(&seen);
    let notify_for_producer = Arc::clone(&notify);

    let _manager_proxy = SagaManagerProps::<SelectiveSaga>::new(move |_components| SelectiveSaga {
        seen: Arc::clone(&seen_for_producer),
        notify: Arc::clone(&notify_for_producer),
    })
    .observing::<OrderPlaced>(Subscription::stream(&upstream_key))
    .spawn(&system)
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

/// A record addressed to an instance that already ended must be dropped
/// without a word on that instance's stream, and must still let the shared
/// subscription move on to the next instance's record.
///
/// It used to leave a durable trace — a dead letter saying "an ended saga
/// received a message". With the queue gone there is nothing to leave: a
/// terminated saga produces no further facts, and writing one here would also
/// buy nothing, because the manager spawns the instance afresh on every
/// arrival and the trace would not stop the next one. What the arrival must
/// still do is let the cursor past it; a record held for an ended instance
/// would starve every instance behind it.
#[tokio::test]
async fn ended_instance_does_not_stall_the_shared_subscription() {
    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::builder(ps)
        .with_persistence(PersistenceProvider::new(InMemoryStore::default()))
        .build();

    let journal = system.persistence().event_store();
    let upstream_key = AggregateId::new("mgr-fanout-orders");
    append_order_placed(&journal, &upstream_key, 1, "ended").await;
    append_order_placed(&journal, &upstream_key, 2, "live").await;

    let ended_instance = SagaId::new(format!("{FANOUT_PREFIX}ended"));
    seed_ended_fact(&journal, &ended_instance, 1).await;

    let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let notify = Arc::new(Notify::new());

    let seen_for_producer = Arc::clone(&seen);
    let notify_for_producer = Arc::clone(&notify);

    let _manager_proxy = SagaManagerProps::<FanOutSaga>::new(move |_components| FanOutSaga {
        seen: Arc::clone(&seen_for_producer),
        notify: Arc::clone(&notify_for_producer),
    })
    .observing::<OrderPlaced>(Subscription::stream(&upstream_key))
    .spawn(&system)
    .await;

    wait_for_seen(&seen, &notify, 1).await;
    tokio::time::sleep(Duration::from_millis(400)).await;

    assert_eq!(
        seen_snapshot(&seen),
        vec!["live".to_owned()],
        "the record for the ended instance must not reach handle(), and it must \
         not stop the record behind it from reaching the live instance"
    );

    // The live instance has already been served by this point, so the ended
    // instance's arrival has had its chance to write.
    let ended_stream = load_stream(&journal, &ended_instance).await;
    assert_eq!(
        describe(&ended_stream),
        vec![format!("{LIFECYCLE_MARKER}@1")],
        "a terminated saga writes nothing further: the stream must still hold \
         only the lifecycle fact that terminated it — no dead letter, and no \
         position either, since a record that reached no `handle` settles \
         nothing the next incarnation could use"
    );
}
