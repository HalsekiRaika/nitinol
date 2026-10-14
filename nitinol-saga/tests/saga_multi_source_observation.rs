//! A saga declares the whole alphabet of inputs it reacts to as one enum, and
//! observes every external variant of it as a source of its own.
//!
//! ```gherkin
//!   Scenario: multi-source observation and correlate routing
//!     Given a saga whose Subscribe enum has two external event types
//!     And both sources are bound to a subscription
//!     When an event of each source arrives
//!     Then correlate routes both to the same instance by its own per-variant rule
//!     And handle receives each one as the typed variant it belongs to
//! ```
//!
//! ## API contract assumed by this file
//!
//! - `#[derive(SagaSubscribe)]` on a user enum whose newtype variants name the
//!   external event types the saga observes.
//! - `Saga::Subscribe` is that enum; `Saga::correlate(&Self::Subscribe)` and
//!   `Saga::handle(Self::Subscribe, ctx)` both take it, so a variant added to
//!   the alphabet is a non-exhaustive `match` at both places.
//! - `SagaManagerProps::new(store, producer)` where `producer` is
//!   `Fn(SagaComponents<S>) -> S`, and `observing::<E>(Subscription)` binds one
//!   declared source per call.  `spawn(&ProcessSystem)` answers a
//!   `SagaManagerProxy<S>`, the handle to the running host.
//! - The instance a record belongs to is never named in the wiring: it is
//!   whatever `Saga::correlate` answers for that record, and the host spawns it
//!   on first claim.  A saga correlating every record to one id is therefore
//!   written exactly as the two-instance one is.
//! - `Saga::apply` is optional, so neither saga below declares it.
//!
//! The same builder reached from a system is `SagaSystemExt::host::<S>(producer)`
//! — one entry point, differing only in where the store comes from and in its
//! `spawn()` taking no argument; it is exercised in
//! `saga_manager_degenerate_single_instance.rs`.
//!
//! If a name differs in the implementation, only the wiring lines need to be
//! reconciled — the asserted behaviour is the requirement.
//!
//! ## Why the assertions do not fix an order
//!
//! Sources are observed independently, and the framework promises no total
//! order across them: there is no "declaration order" of delivery to assert.
//! Every assertion below is therefore made against a *set* of observations, or
//! against one source's own sequence — never against an interleaving of the
//! two.  The shape of the assertion is itself the evidence for that contract.
//!
//! Run with: `cargo test -p nitinol-saga --test saga_multi_source_observation`.

#[path = "common/helpers.rs"]
mod common;
use common::JsonCodec;

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use bytes::Bytes;
use futures_util::TryStreamExt;
use serde::{Deserialize, Serialize};

use nitinol::saga::SagaSubscribe;
use nitinol_eventsource::Event;
use nitinol_persistence::error::{AppendError, LoadError};
use nitinol_persistence::store::{EventStore, EventStream, InMemoryEventStore, Journal};
use nitinol_persistence::{
    AggregateId, AppendOutcome, AppendingEvent, EventType, Family, LoadQuery, LoadedEvent, TypeName,
};
use nitinol_runtime::ProcessSystem;
use nitinol_saga::{Saga, SagaContext, SagaEffect, SagaId, SagaManagerProps, Subscription};

// Domain — two upstream sources, written on two upstream streams.

/// Source A.  Names the business process by `order`.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct OrderPlaced {
    order: String,
}

impl Event for OrderPlaced {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("multi_source"), TypeName::new("OrderPlaced"));
    type Codec = JsonCodec;
}

/// Source B.  Names the same business process under a *different* field, so the
/// two `correlate` arms cannot be collapsed into one rule — a saga that reacted
/// to one shape only could not route this at all.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct StockReserved {
    for_order: String,
}

impl Event for StockReserved {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("multi_source"), TypeName::new("StockReserved"));
    type Codec = JsonCodec;
}

/// The saga's own event.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Noted {
    label: String,
}

impl Event for Noted {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("multi_source"), TypeName::new("Noted"));
    type Codec = JsonCodec;
}

/// The whole alphabet of inputs this saga reacts to: two external sources and
/// no deadline.
#[derive(SagaSubscribe)]
enum Observed {
    Order(OrderPlaced),
    Stock(StockReserved),
}

/// The one business process every record in this file belongs to.
const ORDER: &str = "ord-1";

fn saga_id_of(order: &str) -> SagaId {
    SagaId::new(format!("multi-source-{order}"))
}

/// What one `handle` invocation saw: which variant, and the business key it
/// carried.  The variant label is what makes "received as the typed variant it
/// belongs to" observable — a type-erased delivery could not produce it.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Seen {
    variant: &'static str,
    key: String,
    instance: String,
}

type Handled = Arc<Mutex<Vec<Seen>>>;

struct TranslatingSaga {
    handled: Handled,
}

#[async_trait]
impl Saga for TranslatingSaga {
    type Subscribe = Observed;
    type Event = Noted;
    type Error = std::convert::Infallible;

    fn correlate(input: &Self::Subscribe) -> Option<SagaId> {
        match input {
            Observed::Order(event) => Some(saga_id_of(&event.order)),
            Observed::Stock(event) => Some(saga_id_of(&event.for_order)),
        }
    }

    async fn handle(
        &mut self,
        input: Self::Subscribe,
        ctx: &mut SagaContext,
    ) -> Result<SagaEffect<Self::Event>, Self::Error> {
        let (variant, key) = match input {
            Observed::Order(event) => ("Order", event.order),
            Observed::Stock(event) => ("Stock", event.for_order),
        };
        self.handled
            .lock()
            .expect("handled buffer must not be poisoned")
            .push(Seen {
                variant,
                key: key.clone(),
                instance: ctx.saga_id().as_str().to_owned(),
            });
        Ok(SagaEffect::persist(Noted {
            label: format!("{variant}:{key}"),
        }))
    }
}

// Stores

type Queries = Arc<Mutex<Vec<LoadQuery>>>;

/// Records the [`LoadQuery`] of every poll.
///
/// Two independently-cursored subscriptions each poll their own stream, so the
/// queries this records are where "one subscription per declared source" is
/// observable: a single shared subscription can only carry one stream key, and
/// could never have polled both.
#[derive(Clone)]
struct QueryRecordingStore {
    inner: InMemoryEventStore,
    queries: Queries,
}

#[async_trait]
impl EventStore for QueryRecordingStore {
    async fn append(
        &self,
        key: &str,
        events: Vec<AppendingEvent>,
    ) -> Result<AppendOutcome, AppendError> {
        self.inner.append(key, events).await
    }

    async fn load(&self, query: LoadQuery) -> Result<EventStream<'_>, LoadError> {
        self.queries
            .lock()
            .expect("recorded queries must not be poisoned")
            .push(query.clone());
        self.inner.load(query).await
    }
}

// Helpers

async fn append_order_placed(store: &Journal, stream: &AggregateId, sequence: u64, order: &str) {
    append_raw(
        store,
        stream,
        sequence,
        OrderPlaced::EVENT_TYPE,
        serde_json::to_vec(&OrderPlaced {
            order: order.to_owned(),
        })
        .map(Bytes::from)
        .expect("encode OrderPlaced must succeed"),
    )
    .await;
}

async fn append_stock_reserved(store: &Journal, stream: &AggregateId, sequence: u64, order: &str) {
    append_raw(
        store,
        stream,
        sequence,
        StockReserved::EVENT_TYPE,
        serde_json::to_vec(&StockReserved {
            for_order: order.to_owned(),
        })
        .map(Bytes::from)
        .expect("encode StockReserved must succeed"),
    )
    .await;
}

async fn append_raw(
    store: &Journal,
    stream: &AggregateId,
    sequence: u64,
    event_type: EventType,
    payload: Bytes,
) {
    store
        .append(
            stream.as_str(),
            vec![AppendingEvent {
                sequence,
                event_type,
                payload,
                occurred_at: jiff::Timestamp::now(),
            }],
        )
        .await
        .expect("appending an upstream record must succeed");
}

async fn load_saga_events(store: &Journal, saga_id: &SagaId) -> Vec<LoadedEvent> {
    store
        .load(LoadQuery::by_stream(saga_id))
        .await
        .expect("load saga stream must succeed")
        .try_collect()
        .await
        .expect("collect saga events must succeed")
}

const BARRIER_TIMEOUT: Duration = Duration::from_secs(5);
const POLL_INTERVAL: Duration = Duration::from_millis(25);

async fn wait_for_handled(handled: &Handled, expected: usize) -> Vec<Seen> {
    let deadline = Instant::now() + BARRIER_TIMEOUT;
    loop {
        let current = handled
            .lock()
            .expect("handled buffer must not be poisoned")
            .clone();
        if current.len() >= expected {
            return current;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {expected} handle invocations; saw {current:?}"
        );
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

/// The observations, ordered so the assertion does not depend on which source
/// arrived first.
fn as_set(seen: &[Seen]) -> Vec<Seen> {
    let mut sorted = seen.to_vec();
    sorted.sort();
    sorted
}

// Tests

/// Acceptance scenario: multi-source observation and `correlate` routing.
///
/// Given a saga declaring two external event types in its `Subscribe` enum,
/// and both sources bound,
/// When one record of each source arrives,
/// Then `correlate` routes both to the same instance through its own per-variant
/// rule, and `handle` receives each as the typed variant it belongs to.
#[tokio::test]
async fn both_declared_sources_reach_the_same_instance_as_their_own_variant() {
    let ps = ProcessSystem::new().await;

    let upstream_store = Journal::from(InMemoryEventStore::default());
    let order_stream = AggregateId::new("multi-source-orders");
    let stock_stream = AggregateId::new("multi-source-stock");
    append_order_placed(&upstream_store, &order_stream, 1, ORDER).await;
    append_stock_reserved(&upstream_store, &stock_stream, 1, ORDER).await;

    let saga_store = Journal::from(InMemoryEventStore::default());
    let saga_id = saga_id_of(ORDER);
    let handled: Handled = Arc::new(Mutex::new(Vec::new()));

    let handled_for_producer = Arc::clone(&handled);
    let _proxy = SagaManagerProps::<TranslatingSaga>::new(saga_store.clone(), move |_components| {
        TranslatingSaga {
            handled: Arc::clone(&handled_for_producer),
        }
    })
    .observing::<OrderPlaced>(Subscription::stream(&upstream_store, &order_stream))
    .observing::<StockReserved>(Subscription::stream(&upstream_store, &stock_stream))
    .spawn(&ps)
    .await;

    let seen = wait_for_handled(&handled, 2).await;

    assert_eq!(
        as_set(&seen),
        vec![
            Seen {
                variant: "Order",
                key: ORDER.to_owned(),
                instance: saga_id.as_str().to_owned(),
            },
            Seen {
                variant: "Stock",
                key: ORDER.to_owned(),
                instance: saga_id.as_str().to_owned(),
            },
        ],
        "each declared source must reach `handle` as its own typed variant, and \
         both must be correlated to the one instance their per-variant rules \
         named; saw {seen:?}"
    );

    let events = load_saga_events(&saga_store, &saga_id).await;
    let decisions: Vec<String> = events
        .iter()
        .filter(|event| event.event_type == Noted::EVENT_TYPE)
        .map(|event| {
            serde_json::from_slice::<Noted>(&event.payload)
                .expect("the saga's own event must decode")
                .label
        })
        .collect();
    let mut sorted_decisions = decisions.clone();
    sorted_decisions.sort();
    assert_eq!(
        sorted_decisions,
        vec![format!("Order:{ORDER}"), format!("Stock:{ORDER}")],
        "one decision per observed record, on the one instance's own stream; \
         stream held {decisions:?}"
    );
}

/// Given a saga with two declared sources on two upstream streams,
/// When the host is spawned,
/// Then both streams are polled: each declared source carries a subscription of
/// its own, and no single shared cursor could have read both stream keys.
#[tokio::test]
async fn each_declared_source_is_polled_through_a_subscription_of_its_own() {
    let ps = ProcessSystem::new().await;

    let queries: Queries = Arc::new(Mutex::new(Vec::new()));
    let upstream_store = Journal::from(QueryRecordingStore {
        inner: InMemoryEventStore::default(),
        queries: Arc::clone(&queries),
    });
    let order_stream = AggregateId::new("per-source-orders");
    let stock_stream = AggregateId::new("per-source-stock");
    append_order_placed(&upstream_store, &order_stream, 1, ORDER).await;
    append_stock_reserved(&upstream_store, &stock_stream, 1, ORDER).await;

    let saga_store = Journal::from(InMemoryEventStore::default());
    let handled: Handled = Arc::new(Mutex::new(Vec::new()));

    let handled_for_producer = Arc::clone(&handled);
    let _proxy = SagaManagerProps::<TranslatingSaga>::new(saga_store.clone(), move |_components| {
        TranslatingSaga {
            handled: Arc::clone(&handled_for_producer),
        }
    })
    .observing::<OrderPlaced>(Subscription::stream(&upstream_store, &order_stream))
    .observing::<StockReserved>(Subscription::stream(&upstream_store, &stock_stream))
    .spawn(&ps)
    .await;

    // Both records having been handled is the barrier: whatever polling it took
    // to deliver them has already been recorded.
    wait_for_handled(&handled, 2).await;

    let polled: Vec<String> = queries
        .lock()
        .expect("recorded queries must not be poisoned")
        .iter()
        .filter_map(|query| query.stream_key.clone())
        .collect();
    let polled_order = polled.iter().any(|key| key == order_stream.as_str());
    let polled_stock = polled.iter().any(|key| key == stock_stream.as_str());

    assert!(
        polled_order && polled_stock,
        "both declared sources must be polled in their own right; polled \
         order={polled_order} stock={polled_stock} from {polled:?}"
    );
}
