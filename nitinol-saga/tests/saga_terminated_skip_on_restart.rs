//! A saga that has durably terminated must not be revived.
//!
//! Termination is recorded by a durable `Ended` lifecycle fact in the saga's
//! own event stream (persisted when `SagaEffect::End` is interpreted).  An
//! instance the host spawns afterwards replays that stream in `on_start`,
//! detects the `Ended` fact and starts in the terminal lifecycle: every record
//! routed to it is reported and dropped, so `Saga::handle` is never invoked
//! again, no matter how many routed upstream events arrive.
//!
//! The three tests below pin the contract from both directions:
//! - baseline: a saga with *no* `Ended` fact DOES handle a routed event
//!   (proves the harness actually delivers events);
//! - skip: a saga whose stream carries a seeded `Ended` fact does NOT handle
//!   the routed event;
//! - end-to-end: a saga that reaches `End` (persisting the fact itself) is
//!   skipped when a fresh instance replays the same store.
//!
//! Which identity the fact is written down under — `nitinol.saga.lifecycle`
//! rather than an envelope shared with a transport mechanism — is
//! `saga_lifecycle_ended_declaration.rs`; here it is only the signal.

#[path = "common/helpers.rs"]
mod common;
use common::{encode_lifecycle_ended, JsonCodec, LIFECYCLE_MARKER};

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use futures_util::TryStreamExt;
use serde::{Deserialize, Serialize};
use tokio::sync::Notify;

use nitinol_eventsource::{system::EventSourceSystem, Event};
use nitinol_persistence::provider::PersistenceProvider;
use nitinol_persistence::store::{EventStore, InMemoryEventStore, InMemoryStore};
use nitinol_persistence::{
    AggregateId, AppendingEvent, EventType, Family, LoadQuery, LoadedEvent, TypeName,
};
use nitinol_runtime::ProcessSystem;
use nitinol_saga::{
    Saga, SagaContext, SagaEffect, SagaId, SagaManagerProps, SagaSubscribe, Subscription,
};

// Domain types

#[derive(Clone, Debug, Serialize, Deserialize)]
struct OrderPlaced {
    sku: String,
}

impl Event for OrderPlaced {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("terminated_skip"), TypeName::new("OrderPlaced"));
    type Codec = JsonCodec;
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ReservationRequested {
    sku: String,
}

impl Event for ReservationRequested {
    const EVENT_TYPE: EventType = EventType::new(
        Family::new("terminated_skip"),
        TypeName::new("ReservationRequested"),
    );
    type Codec = JsonCodec;
}

// Sagas

/// Correlation rule shared by both sagas in this file.  The respawn test hands
/// one saga stream from [`EndOnFirstSaga`] to [`RecordingSaga`], so the two
/// types must name the same instance for the second incarnation to be the same
/// saga at all.
const SAGA_ID: &str = "terminated-skip-saga";

/// The whole alphabet both sagas in this file react to: one observed source, no
/// deadline.  Shared, because the two types observe the same upstream event.
#[derive(SagaSubscribe)]
enum Observed {
    Order(OrderPlaced),
}

/// Records every `handle` call and never terminates.  Used to observe whether a
/// record the host routes to the instance reaches the saga at all.
struct RecordingSaga {
    handle_count: Arc<AtomicUsize>,
    handled: Arc<Notify>,
}

#[async_trait]
impl Saga for RecordingSaga {
    type Subscribe = Observed;
    type Event = ReservationRequested;
    type Error = std::convert::Infallible;

    fn correlate(_input: &Self::Subscribe) -> Option<SagaId> {
        Some(SagaId::new(SAGA_ID))
    }

    async fn handle(
        &mut self,
        _input: Self::Subscribe,
        _ctx: &mut SagaContext,
    ) -> Result<SagaEffect<Self::Event>, Self::Error> {
        self.handle_count.fetch_add(1, Ordering::SeqCst);
        self.handled.notify_one();
        Ok(SagaEffect::None)
    }
}

/// Persists one event and immediately terminates via `End` on the first
/// routed upstream event.  Used to drive the durable `Ended` lifecycle fact for
/// the end-to-end restart test.
struct EndOnFirstSaga {
    handle_count: Arc<AtomicUsize>,
}

#[async_trait]
impl Saga for EndOnFirstSaga {
    type Subscribe = Observed;
    type Event = ReservationRequested;
    type Error = std::convert::Infallible;

    fn correlate(_input: &Self::Subscribe) -> Option<SagaId> {
        Some(SagaId::new(SAGA_ID))
    }

    async fn handle(
        &mut self,
        input: Self::Subscribe,
        _ctx: &mut SagaContext,
    ) -> Result<SagaEffect<Self::Event>, Self::Error> {
        let Observed::Order(event) = input;
        self.handle_count.fetch_add(1, Ordering::SeqCst);
        Ok(SagaEffect::persist(ReservationRequested { sku: event.sku }).then_end())
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

/// Seed a raw durable `Ended` lifecycle fact at `sequence` on the saga's own
/// stream, simulating a saga that terminated in a previous incarnation.
async fn seed_ended_fact(store: &Arc<dyn EventStore>, saga_id: &SagaId, sequence: u64) {
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

async fn load_saga_events(store: &Arc<dyn EventStore>, saga_id: &SagaId) -> Vec<LoadedEvent> {
    store
        .load(LoadQuery::by_stream(saga_id))
        .await
        .expect("load saga stream must succeed")
        .try_collect()
        .await
        .expect("collect saga events must succeed")
}

#[allow(clippy::type_complexity)]
async fn spawn_recording_saga(
    system: &EventSourceSystem<InMemoryStore>,
    saga_store: &Arc<dyn EventStore>,
    upstream_store: &Arc<dyn EventStore>,
    upstream_key: &str,
    handle_count: Arc<AtomicUsize>,
    handled: Arc<Notify>,
) -> nitinol_saga::SagaManagerProxy<RecordingSaga> {
    SagaManagerProps::<RecordingSaga>::new(Arc::clone(saga_store), move |_components| {
        RecordingSaga {
            handle_count: Arc::clone(&handle_count),
            handled: Arc::clone(&handled),
        }
    })
    .observing::<OrderPlaced>(Subscription::stream(upstream_store, upstream_key))
    .spawn(system.process_system())
    .await
}

// Tests

/// Baseline / positive control: a saga with a fresh (empty) stream must handle
/// the routed upstream event.  Without this, the skip test below could pass
/// simply because the harness never delivered the event.
#[tokio::test]
async fn non_terminated_saga_handles_routed_event() {
    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::builder(ps)
        .with_persistence(PersistenceProvider::new(InMemoryStore::default()))
        .build();

    let upstream_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let order_id = AggregateId::new("baseline-order");
    append_order_placed(&upstream_store, &order_id, 1, "SKU-1").await;

    let saga_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let handle_count = Arc::new(AtomicUsize::new(0));
    let handled = Arc::new(Notify::new());

    let _saga_proxy = spawn_recording_saga(
        &system,
        &saga_store,
        &upstream_store,
        order_id.as_str(),
        Arc::clone(&handle_count),
        Arc::clone(&handled),
    )
    .await;

    tokio::time::timeout(Duration::from_secs(3), handled.notified())
        .await
        .expect("a saga with no Ended marker must handle the routed upstream event");

    assert_eq!(
        handle_count.load(Ordering::SeqCst),
        1,
        "the non-terminated saga must handle exactly the one routed event"
    );
}

/// When the saga stream already carries a durable `Ended` lifecycle fact,
/// `on_start` must put the instance in the terminal lifecycle, so `handle` is
/// never called for a routed upstream event.
#[tokio::test]
async fn terminated_saga_drops_routed_records_and_never_handles() {
    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::builder(ps)
        .with_persistence(PersistenceProvider::new(InMemoryStore::default()))
        .build();

    let upstream_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let order_id = AggregateId::new("terminated-order");
    append_order_placed(&upstream_store, &order_id, 1, "SKU-1").await;

    let saga_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let saga_id = SagaId::new(SAGA_ID);

    // Simulate a saga that ended in a previous incarnation.
    seed_ended_fact(&saga_store, &saga_id, 1).await;

    let handle_count = Arc::new(AtomicUsize::new(0));
    let handled = Arc::new(Notify::new());

    let _saga_proxy = spawn_recording_saga(
        &system,
        &saga_store,
        &upstream_store,
        order_id.as_str(),
        Arc::clone(&handle_count),
        Arc::clone(&handled),
    )
    .await;

    // Give the host ample time to (incorrectly) replay and hand the routed
    // event to the saga if the terminated-skip guard were missing.
    tokio::time::sleep(Duration::from_millis(750)).await;

    assert_eq!(
        handle_count.load(Ordering::SeqCst),
        0,
        "an instance whose stream carries a durable Ended lifecycle fact must \
         drop whatever the host routes to it; Saga::handle must never be invoked"
    );
}

/// End-to-end: a saga that reaches `End` persists the durable `Ended` lifecycle
/// fact itself; a fresh instance replaying the same store must then be skipped.
#[tokio::test]
async fn saga_that_ended_is_skipped_when_respawned_over_same_store() {
    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::builder(ps)
        .with_persistence(PersistenceProvider::new(InMemoryStore::default()))
        .build();

    let upstream_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let order_id = AggregateId::new("respawn-order");
    append_order_placed(&upstream_store, &order_id, 1, "SKU-FIRST").await;

    let saga_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let saga_id = SagaId::new(SAGA_ID);

    // First incarnation: handle the first event and terminate via End.
    let first_count = Arc::new(AtomicUsize::new(0));
    let first_count_clone = Arc::clone(&first_count);
    let first_proxy =
        SagaManagerProps::<EndOnFirstSaga>::new(Arc::clone(&saga_store), move |_components| {
            EndOnFirstSaga {
                handle_count: Arc::clone(&first_count_clone),
            }
        })
        .observing::<OrderPlaced>(Subscription::stream(&upstream_store, &order_id))
        .spawn(system.process_system())
        .await;

    // Wait until the End interpretation has durably persisted the Ended fact.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let events = load_saga_events(&saga_store, &saga_id).await;
        let ended = events
            .iter()
            .filter(|e| {
                matches!(
                    common::lifecycle_kind_of(e),
                    Some(common::LifecycleKind::Ended(_))
                )
            })
            .count();
        if ended >= 1 {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for the first saga to persist its Ended fact; events: {:?}",
            events
                .iter()
                .map(|e| e.event_type.to_string())
                .collect::<Vec<_>>()
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert_eq!(
        first_count.load(Ordering::SeqCst),
        1,
        "the first incarnation must have handled exactly the one event before End"
    );
    drop(first_proxy);

    // Second incarnation over the SAME saga store — must be skipped.
    append_order_placed(&upstream_store, &order_id, 2, "SKU-SECOND").await;

    let second_count = Arc::new(AtomicUsize::new(0));
    let second_handled = Arc::new(Notify::new());
    let _second_proxy = spawn_recording_saga(
        &system,
        &saga_store,
        &upstream_store,
        order_id.as_str(),
        Arc::clone(&second_count),
        Arc::clone(&second_handled),
    )
    .await;

    tokio::time::sleep(Duration::from_millis(750)).await;

    assert_eq!(
        second_count.load(Ordering::SeqCst),
        0,
        "a fresh instance replaying a stream that already ended must be \
         skipped; Saga::handle must not run for any subsequent upstream event"
    );
}
