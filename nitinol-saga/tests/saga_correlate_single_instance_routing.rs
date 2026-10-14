//! Which instance a record is delivered to is decided by [`Saga::correlate`],
//! and by nothing at the wiring site.  (Contracts `CT-03`, `CT-02`, `CT-09`.)
//!
//! The runtime consults the saga *type* for every decoded upstream record and
//! routes by the answer:
//!
//! | `correlate(&event)` | outcome                                        |
//! |---------------------|------------------------------------------------|
//! | `None`              | nobody's work: ignored, nothing written        |
//! | `Some(id)`          | delivered to [`Saga::handle`] on *that* id and |
//! |                     | on no other instance                           |
//!
//! Both classes travel through **one** upstream stream in a single test so that
//! an implementation satisfying only part of the table is identified: treating
//! `None` as some instance's work admits the `NONE-*` events, and routing by
//! anything other than the answer sends a `MINE-*` event to the instance the
//! `OTHER-*` events belong to, or the other way round.  The instance a record
//! reached is read from the [`SagaContext`] `handle` was given, so a run that
//! delivered the right count to the wrong instance is not mistaken for a pass.
//!
//! "Ignored" is a *silent* outcome — nothing at all is written for a record no
//! instance claims, pinned separately below so that an implementation which
//! leaves a durable trace for every record it passes over is rejected rather
//! than passing on the delivery assertion alone.
//!
//! `CT-02`: every wiring here calls `observing::<OrderPlaced>` with one
//! argument (a [`Subscription`]) — the upstream format is named by the declared
//! source itself, not handed to the builder.  A builder that still takes a
//! routing argument — positionally or as a trailing `Option` — fails to compile
//! against these call sites.

#[path = "common/helpers.rs"]
mod common;
use common::{readable_consumed_fact_of, JsonCodec, CONSUMED_MARKER};

use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use futures_util::TryStreamExt;
use serde::{Deserialize, Serialize};
use tokio::sync::Notify;

use nitinol_eventsource::system::EventSourceSystem;
use nitinol_eventsource::Event;
use nitinol_persistence::provider::PersistenceProvider;
use nitinol_persistence::store::{InMemoryEventStore, InMemoryStore, Journal};
use nitinol_persistence::{
    AggregateId, AppendingEvent, EventType, Family, LoadQuery, LoadedEvent, TypeName,
};
use nitinol_runtime::ProcessSystem;
use nitinol_saga::{
    Saga, SagaContext, SagaEffect, SagaId, SagaManagerProps, SagaSubscribe, Subscription,
};

// Correlation rule — one definition point, shared by `correlate` and by the
// assertions below.  Both ids name instances of the same saga type, built by
// the same producer: what tells them apart is only which records `correlate`
// sends where, which is the whole of the claim.

const OWN_SAGA_ID: &str = "correlate-routing-own";
const FOREIGN_SAGA_ID: &str = "correlate-routing-foreign";

#[derive(Clone, Debug, Serialize, Deserialize)]
struct OrderPlaced {
    sku: String,
}

impl Event for OrderPlaced {
    const EVENT_TYPE: EventType = EventType::new(
        Family::new("saga.correlate.routing"),
        TypeName::new("OrderPlaced"),
    );
    type Codec = JsonCodec;
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
    type Codec = JsonCodec;
}

/// The whole alphabet this saga reacts to: one observed source, no deadline.
#[derive(SagaSubscribe)]
enum Observed {
    Order(OrderPlaced),
}

/// Records every event that actually reaches `handle`, and the instance it
/// reached.
///
/// Both instances are built by the one producer the host was given, so they
/// share this buffer: which of them a record was handed is read from the
/// [`SagaContext`] rather than inferred from which buffer grew, and a run that
/// delivered a `MINE-*` record to the foreign instance is therefore a failure
/// rather than a pass on the count.
struct CorrelationSaga {
    captured: Arc<Mutex<Vec<String>>>,
    notify: Arc<Notify>,
}

#[async_trait]
impl Saga for CorrelationSaga {
    type Subscribe = Observed;
    type Event = ReservationRequested;
    type Error = std::convert::Infallible;

    /// Derives the owning instance from the SKU prefix so that one upstream
    /// stream can carry all three correlation outcomes.
    fn correlate(input: &Self::Subscribe) -> Option<SagaId> {
        match input {
            Observed::Order(event) => {
                if event.sku.starts_with("MINE-") {
                    Some(SagaId::new(OWN_SAGA_ID))
                } else if event.sku.starts_with("OTHER-") {
                    Some(SagaId::new(FOREIGN_SAGA_ID))
                } else {
                    None
                }
            }
        }
    }

    async fn handle(
        &mut self,
        input: Self::Subscribe,
        ctx: &mut SagaContext,
    ) -> Result<SagaEffect<Self::Event>, Self::Error> {
        let Observed::Order(event) = input;
        self.captured
            .lock()
            .expect("captured mutex is never poisoned: no holder panics while the guard is alive")
            .push(format!("{}@{}", event.sku, ctx.saga_id().as_str()));
        self.notify.notify_one();
        Ok(SagaEffect::None)
    }
}

// Helpers

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

/// The upstream stream every test in this file uses: the three correlation
/// classes interleaved, with a declined event *last* so that a run which
/// wrongly accepts it is still observed after the wait for the matching ones.
async fn append_mixed_correlation_stream(store: &Journal, agg_id: &AggregateId) {
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

async fn load_saga_events(store: &Journal, saga_id: &SagaId) -> Vec<LoadedEvent> {
    store
        .load(LoadQuery::by_stream(saga_id))
        .await
        .expect("load saga stream must succeed")
        .try_collect()
        .await
        .expect("collect saga events must succeed")
}

/// Host the saga under test over `upstream_store`, observing `agg_id` from the
/// beginning.  No routing argument is passed, and no instance is named —
/// correlation lives on `CorrelationSaga`.
async fn spawn_correlation_saga(
    system: &EventSourceSystem<InMemoryStore>,
    upstream_store: &Journal,
    agg_id: &AggregateId,
    saga_store: Journal,
    captured: &Arc<Mutex<Vec<String>>>,
    notify: &Arc<Notify>,
) {
    let captured_for_producer = Arc::clone(captured);
    let notify_for_producer = Arc::clone(notify);

    let _host_proxy =
        SagaManagerProps::<CorrelationSaga>::new(saga_store, move |_components| CorrelationSaga {
            captured: Arc::clone(&captured_for_producer),
            notify: Arc::clone(&notify_for_producer),
        })
        .observing::<OrderPlaced>(Subscription::stream(upstream_store, agg_id))
        .spawn(system.process_system())
        .await;
}

// Tests

/// Given one upstream stream carrying events that correlate to nobody and to
/// two different instances,
/// when the host observes it,
/// then each event reaches `Saga::handle` on exactly the instance its
/// `correlate` answer names, in upstream order, and the ones that name nobody
/// reach none.
#[tokio::test]
async fn each_record_reaches_handle_on_the_instance_correlate_names() {
    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::builder(ps)
        .with_persistence(PersistenceProvider::new(InMemoryStore::default()))
        .build();

    let upstream_store = Journal::from(InMemoryEventStore::default());
    let order_id = AggregateId::new("correlate-routing-order");
    append_mixed_correlation_stream(&upstream_store, &order_id).await;

    let saga_store = Journal::from(InMemoryEventStore::default());
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

    wait_for_count(&captured, &notify, 4).await;
    // The stream ends with a `NONE-*` event; give it time to be delivered and
    // passed over so a run that wrongly hands it to an instance is observed
    // here.
    tokio::time::sleep(Duration::from_millis(300)).await;

    assert_eq!(
        captured_snapshot(&captured),
        vec![
            format!("OTHER-1@{FOREIGN_SAGA_ID}"),
            format!("MINE-1@{OWN_SAGA_ID}"),
            format!("MINE-2@{OWN_SAGA_ID}"),
            format!("OTHER-2@{FOREIGN_SAGA_ID}"),
        ],
        "`handle` must run on the instance each record's correlate answer \
         names, in upstream order, and on no other: `NONE-*` names nobody and \
         must reach neither instance"
    );
}

/// Given the same stream,
/// when a record names no instance,
/// then passing over it is silent — nothing is written to either instance's own
/// stream for it, not even a position claiming to have consumed it.
#[tokio::test]
async fn a_record_that_names_no_instance_is_silent_and_writes_nothing() {
    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::builder(ps)
        .with_persistence(PersistenceProvider::new(InMemoryStore::default()))
        .build();

    let upstream_store = Journal::from(InMemoryEventStore::default());
    let order_id = AggregateId::new("correlate-routing-silent-order");
    append_mixed_correlation_stream(&upstream_store, &order_id).await;

    let saga_store = Journal::from(InMemoryEventStore::default());
    let captured: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let notify = Arc::new(Notify::new());

    spawn_correlation_saga(
        &system,
        &upstream_store,
        &order_id,
        saga_store.clone(),
        &captured,
        &notify,
    )
    .await;

    // Reaching all four claimed records proves the three unclaimed ones were
    // fetched and evaluated, so the assertions below are about work that
    // happened.
    wait_for_count(&captured, &notify, 4).await;
    tokio::time::sleep(Duration::from_millis(300)).await;

    for (instance, claimed) in [(OWN_SAGA_ID, vec![3, 5]), (FOREIGN_SAGA_ID, vec![2, 6])] {
        let events = load_saga_events(&saga_store, &SagaId::new(instance)).await;
        let written: Vec<String> = events
            .iter()
            .map(|e| format!("{}@{}", e.event_type, e.sequence))
            .collect();

        assert_eq!(
            events
                .iter()
                .filter(|e| e.event_type == CONSUMED_MARKER)
                .count(),
            events.len(),
            "the records `{instance}` was handed decide nothing, so a position \
             each is the whole of what it writes; got {written:?}"
        );
        assert_eq!(
            events
                .iter()
                .filter_map(readable_consumed_fact_of)
                .map(|fact| fact.sequence)
                .collect::<Vec<_>>(),
            claimed,
            "a record naming no instance is nobody's business, so no instance \
             writes a position for it — and a record naming the other instance \
             is not `{instance}`'s to vouch for either; got {written:?}"
        );
    }
}
