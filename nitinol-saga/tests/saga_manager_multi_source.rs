//! A manager owns one subscription *per declared source*, so each record of
//! each source is decoded once no matter how far the correlation fans out.
//!
//! `saga_manager_single_subscription.rs` pins the one-source form of this: `M`
//! records across `N` instances cost `M` decodes, not `M × N`. An alphabet with
//! several sources is where that could quietly stop holding — by giving each
//! instance a poller of its own again, or by having one poller read every source
//! through a single cursor. This file pins the generalisation: two sources, two
//! correlation ids, and a decode count per source that the fan-out does not
//! multiply.
//!
//! It also pins the manager side of the routing contract: a record arrives at
//! the instance `Saga::correlate` names, as the variant of the alphabet it
//! belongs to. Nothing here asserts an order *between* the two sources — there
//! is deliberately no such promise, so every assertion compares sets.
//!
//! The decode counters are process-wide `static`s (a `Codec` is a pair of
//! associated functions and cannot carry instance state), which is why this
//! binary deliberately holds a single test.
//!
//! Run with: `cargo test -p nitinol-saga --test saga_manager_multi_source`.

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

use nitinol_eventsource::codec::Codec;
use nitinol_eventsource::{system::EventSourceSystem, Event};
use nitinol_persistence::store::{EventStore, InMemoryEventStore};
use nitinol_persistence::{AggregateId, AppendingEvent, EventType, Family, TypeName};
use nitinol_runtime::ProcessSystem;
use nitinol_saga::{
    Saga, SagaContext, SagaEffect, SagaId, SagaManagerProps, SagaSubscribe, Subscription,
};

const INSTANCE_PREFIX: &str = "mgr-multi-";

/// The orders each source carries, so both sources fan out to the same two
/// instances and the decode counts below are read against a real fan-out.
const ORDERS: [&str; 2] = ["A", "B"];

/// Decodes performed by each source's codec, counted separately: a single
/// shared cursor over both sources would starve one of them, which shows up as
/// a count that never reaches its source's record count.
static ORDER_DECODES: AtomicUsize = AtomicUsize::new(0);
static STOCK_DECODES: AtomicUsize = AtomicUsize::new(0);

// Domain types — one event type per declared source.

#[derive(Clone, Debug, Serialize, Deserialize)]
struct OrderPlaced {
    order: String,
}

impl Event for OrderPlaced {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("mgr.multi"), TypeName::new("OrderPlaced"));
    type Codec = CountingOrderCodec;
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct StockReserved {
    order: String,
}

impl Event for StockReserved {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("mgr.multi"), TypeName::new("StockReserved"));
    type Codec = CountingStockCodec;
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Noted {
    label: String,
}

impl Event for Noted {
    const EVENT_TYPE: EventType = EventType::new(Family::new("mgr.multi"), TypeName::new("Noted"));
    type Codec = JsonCodec;
}

/// JSON codec bound to one source's event type alone, so an unrelated decode
/// cannot inflate that source's counter.
struct CountingOrderCodec;

impl Codec<OrderPlaced> for CountingOrderCodec {
    type Error = serde_json::Error;

    fn encode(event: &OrderPlaced) -> Result<Bytes, Self::Error> {
        serde_json::to_vec(event).map(Bytes::from)
    }

    fn decode(payload: &[u8]) -> Result<OrderPlaced, Self::Error> {
        ORDER_DECODES.fetch_add(1, Ordering::SeqCst);
        serde_json::from_slice(payload)
    }
}

struct CountingStockCodec;

impl Codec<StockReserved> for CountingStockCodec {
    type Error = serde_json::Error;

    fn encode(event: &StockReserved) -> Result<Bytes, Self::Error> {
        serde_json::to_vec(event).map(Bytes::from)
    }

    fn decode(payload: &[u8]) -> Result<StockReserved, Self::Error> {
        STOCK_DECODES.fetch_add(1, Ordering::SeqCst);
        serde_json::from_slice(payload)
    }
}

// Saga under test

/// Two observed sources and no deadline: the alphabet a coordinator watching two
/// upstreams declares.
#[derive(SagaSubscribe)]
enum Observed {
    Order(OrderPlaced),
    Stock(StockReserved),
}

struct RecordingSaga {
    seen: Arc<Mutex<Vec<(String, String)>>>,
    notify: Arc<Notify>,
}

#[async_trait]
impl Saga for RecordingSaga {
    type Subscribe = Observed;
    type Event = Noted;
    type Error = std::convert::Infallible;

    fn correlate(input: &Self::Subscribe) -> Option<SagaId> {
        // A correlation rule per variant, over one id space: the two sources
        // speak about the same business process from different streams.
        let order = match input {
            Observed::Order(event) => &event.order,
            Observed::Stock(event) => &event.order,
        };
        Some(SagaId::new(format!("{INSTANCE_PREFIX}{order}")))
    }

    async fn handle(
        &mut self,
        input: Self::Subscribe,
        ctx: &mut SagaContext,
    ) -> Result<SagaEffect<Self::Event>, Self::Error> {
        // The variant label is the observation: a record must arrive as the
        // variant of the alphabet it belongs to, not as the other source's.
        let variant = match input {
            Observed::Order(_) => "order",
            Observed::Stock(_) => "stock",
        };
        self.seen
            .lock()
            .expect("seen mutex is never poisoned: no holder panics while the guard is alive")
            .push((ctx.saga_id().as_str().to_owned(), variant.to_owned()));
        self.notify.notify_one();
        Ok(SagaEffect::None)
    }
}

// Helpers

async fn append<E: Event + Serialize>(
    store: &Arc<dyn EventStore>,
    upstream_key: &AggregateId,
    sequence: u64,
    event: &E,
) {
    let payload = serde_json::to_vec(event)
        .map(Bytes::from)
        .expect("encoding a test event must succeed");
    store
        .append(
            upstream_key.as_str(),
            vec![AppendingEvent {
                sequence,
                event_type: E::EVENT_TYPE,
                payload,
                occurred_at: jiff::Timestamp::now(),
            }],
        )
        .await
        .expect("appending a test event must succeed");
}

fn seen_snapshot(seen: &Arc<Mutex<Vec<(String, String)>>>) -> Vec<(String, String)> {
    seen.lock()
        .expect("seen mutex is never poisoned: no holder panics while the guard is alive")
        .clone()
}

/// The observations as a set: the two sources are polled independently, so their
/// arrival order is not a promise this file may lean on.
fn as_set(seen: &Arc<Mutex<Vec<(String, String)>>>) -> Vec<(String, String)> {
    let mut set = seen_snapshot(seen);
    set.sort();
    set
}

async fn wait_for_seen(
    seen: &Arc<Mutex<Vec<(String, String)>>>,
    notify: &Arc<Notify>,
    expected: usize,
) {
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

// Test

#[tokio::test]
async fn a_manager_decodes_each_source_once_and_routes_both_to_the_same_instances() {
    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::builder(ps).build();

    // Each source reads a stream key of its own, which is the ordinary shape:
    // two aggregates a coordinator watches.
    let order_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let order_key = AggregateId::new("mgr-multi-orders");
    let stock_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let stock_key = AggregateId::new("mgr-multi-stock");

    for (index, order) in ORDERS.iter().enumerate() {
        let sequence = u64::try_from(index + 1).expect("test sequence fits in u64");
        append(
            &order_store,
            &order_key,
            sequence,
            &OrderPlaced {
                order: (*order).to_owned(),
            },
        )
        .await;
        append(
            &stock_store,
            &stock_key,
            sequence,
            &StockReserved {
                order: (*order).to_owned(),
            },
        )
        .await;
    }

    let saga_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());

    let seen: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(Vec::new()));
    let notify = Arc::new(Notify::new());
    let instances_spawned = Arc::new(AtomicUsize::new(0));

    let seen_for_producer = Arc::clone(&seen);
    let notify_for_producer = Arc::clone(&notify);
    let spawned_for_producer = Arc::clone(&instances_spawned);

    let _manager_proxy =
        SagaManagerProps::<RecordingSaga>::new(Arc::clone(&saga_store), move |_components| {
            spawned_for_producer.fetch_add(1, Ordering::SeqCst);
            RecordingSaga {
                seen: Arc::clone(&seen_for_producer),
                notify: Arc::clone(&notify_for_producer),
            }
        })
        .subscribed_to::<OrderPlaced>(Subscription::stream(&order_store, &order_key))
        .subscribed_to::<StockReserved>(Subscription::stream(&stock_store, &stock_key))
        .spawn(system.process_system())
        .await;

    wait_for_seen(&seen, &notify, ORDERS.len() * 2).await;
    // A per-instance poller keeps decoding after the last delivery, so settle
    // past one poll interval before reading the counters.
    tokio::time::sleep(Duration::from_millis(500)).await;

    assert_eq!(
        as_set(&seen),
        vec![
            (format!("{INSTANCE_PREFIX}A"), "order".to_owned()),
            (format!("{INSTANCE_PREFIX}A"), "stock".to_owned()),
            (format!("{INSTANCE_PREFIX}B"), "order".to_owned()),
            (format!("{INSTANCE_PREFIX}B"), "stock".to_owned()),
        ],
        "each record must reach the instance its own variant's correlation rule \
         names, as that variant"
    );
    assert_eq!(
        instances_spawned.load(Ordering::SeqCst),
        2,
        "the two correlation ids must produce two instances — without the \
         fan-out the decode counts below would not be testing anything"
    );
    assert_eq!(
        (
            ORDER_DECODES.load(Ordering::SeqCst),
            STOCK_DECODES.load(Ordering::SeqCst),
        ),
        (ORDERS.len(), ORDERS.len()),
        "the manager must hold one subscription per declared source and decode \
         each record of each source exactly once; a poller per instance would \
         decode {} × {} times per source",
        ORDERS.len(),
        2
    );
}
