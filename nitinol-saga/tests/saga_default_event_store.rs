//! A saga spawned from a system that already holds a default [`EventStore`].
//!
//! A saga touches two stores: the one it journals its own events onto, and the
//! one it polls for upstream records.  Both are resolved from the system's
//! default when the caller omits them — `spawn_saga(saga_id, producer)` for the
//! journal, `system.subscription(&key)` for the upstream — and both stay
//! overridable per spawn, via `spawn_saga_with_store` and
//! `Subscription::stream`.
//!
//! The two directions are pinned separately: a wiring that resolves the journal
//! from the default but the upstream from somewhere else (or the reverse) still
//! passes any test that only checks one of them.

#[path = "common/helpers.rs"]
mod common;
use common::JsonCodec;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use futures_util::TryStreamExt;
use serde::{Deserialize, Serialize};
use tokio::sync::Notify;

use nitinol_eventsource::{system::EventSourceSystem, Event};
use nitinol_persistence::store::{EventStore, InMemoryEventStore};
use nitinol_persistence::{
    AggregateId, AppendingEvent, EventType, Family, LoadQuery, LoadedEvent, TypeName,
};
use nitinol_runtime::ProcessSystem;
use nitinol_saga::{Saga, SagaContext, SagaDefaultStoreExt, SagaEffect, SagaId, Subscription};

// Fixtures

#[derive(Clone, Debug, Serialize, Deserialize)]
struct OrderPlaced {
    sku: String,
}

impl Event for OrderPlaced {
    const EVENT_TYPE: EventType = EventType::new(
        Family::new("saga.default.store"),
        TypeName::new("OrderPlaced"),
    );
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ReservationRequested {
    sku: String,
}

impl Event for ReservationRequested {
    const EVENT_TYPE: EventType = EventType::new(
        Family::new("saga.default.store"),
        TypeName::new("ReservationRequested"),
    );
}

/// Correlation rule of [`RecordingSaga`]: every order belongs to the single
/// reservation process each test in this file spawns.
const RECORDING_SAGA_ID: &str = "saga-default-store-reservation";

struct RecordingSaga {
    handled: Arc<Mutex<Vec<String>>>,
    notify: Arc<Notify>,
}

#[async_trait]
impl Saga for RecordingSaga {
    type SubscribedEvent = OrderPlaced;
    type Event = ReservationRequested;
    type ScheduledMessage = ();
    type Error = std::convert::Infallible;

    fn correlate(_event: &Self::SubscribedEvent) -> Option<SagaId> {
        Some(SagaId::new(RECORDING_SAGA_ID))
    }

    fn apply(&mut self, _event: Self::Event) {}

    async fn handle(
        &mut self,
        event: Self::SubscribedEvent,
        _ctx: &mut SagaContext,
    ) -> Result<SagaEffect<Self::Event>, Self::Error> {
        self.handled
            .lock()
            .expect("handled mutex is never poisoned: no holder panics while the guard is alive")
            .push(event.sku.clone());
        self.notify.notify_one();
        Ok(SagaEffect::persist(ReservationRequested { sku: event.sku }))
    }
}

// Helpers

async fn append_order_placed(
    store: &Arc<dyn EventStore>,
    key: &AggregateId,
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
            key.as_str(),
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

async fn loaded_stream(store: &Arc<dyn EventStore>, key: &SagaId) -> Vec<LoadedEvent> {
    store
        .load(LoadQuery::by_stream(key))
        .await
        .expect("saga event store load must succeed")
        .try_collect()
        .await
        .expect("collect saga events must succeed")
}

/// SKUs of the saga's own events on its own stream in `store`.
///
/// Reading a *named* store back is what tells the journal stores apart — the
/// `SagaProxy` a spawn returns looks the same whichever store it writes to.
async fn persisted_skus(store: &Arc<dyn EventStore>, saga_id: &SagaId) -> Vec<String> {
    loaded_stream(store, saga_id)
        .await
        .iter()
        .filter(|event| event.event_type == ReservationRequested::EVENT_TYPE)
        .map(|event| {
            serde_json::from_slice::<ReservationRequested>(&event.payload)
                .expect("the saga's own event must be encoded with the system codec")
                .sku
        })
        .collect()
}

async fn wait_for_persisted(store: &Arc<dyn EventStore>, saga_id: &SagaId) -> Vec<String> {
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

// Default store: the saga's own journal

/// `spawn_saga(saga_id, producer)` — no journal argument — must write the
/// saga's own events onto the store the system was built with.
#[tokio::test]
async fn store_less_spawn_saga_journals_onto_the_system_default_store() {
    // Given: a system holding one default store, and an unrelated upstream
    let ps = ProcessSystem::new().await;
    let default_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let system = EventSourceSystem::builder(ps)
        .with_codec::<JsonCodec>()
        .with_event_store(Arc::clone(&default_store))
        .build();

    let upstream_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let order_id = AggregateId::new("saga-default-journal-orders");
    let saga_id = SagaId::new(RECORDING_SAGA_ID);

    let handled: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let notify = Arc::new(Notify::new());
    let handled_for_producer = Arc::clone(&handled);
    let notify_for_producer = Arc::clone(&notify);

    // When: spawning without naming a journal store
    let _saga_proxy = system
        .spawn_saga(saga_id.clone(), move || RecordingSaga {
            handled: Arc::clone(&handled_for_producer),
            notify: Arc::clone(&notify_for_producer),
        })
        .subscribed_to(Subscription::stream(&upstream_store, &order_id))
        .spawn()
        .await;

    append_order_placed(&upstream_store, &order_id, 1, "SKU-DEFAULT-JOURNAL").await;
    wait_for_handled(&handled, &notify, 1).await;

    // Then
    assert_eq!(
        wait_for_persisted(&default_store, &saga_id).await,
        vec!["SKU-DEFAULT-JOURNAL".to_owned()],
        "a store-less spawn_saga must journal onto the store the builder held"
    );
}

// Default store: the upstream subscription

/// `system.subscription(&key)` must poll the system's default store, so a saga
/// fed by an aggregate living in that same store needs no `Arc` at the call
/// site.
#[tokio::test]
async fn system_subscription_polls_the_system_default_store() {
    // Given: the journal is named explicitly, so the default store is only
    // reachable through the subscription under test
    let ps = ProcessSystem::new().await;
    let default_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let system = EventSourceSystem::builder(ps)
        .with_codec::<JsonCodec>()
        .with_event_store(Arc::clone(&default_store))
        .build();

    let journal_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let order_id = AggregateId::new("saga-default-subscription-orders");

    let handled: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let notify = Arc::new(Notify::new());
    let handled_for_producer = Arc::clone(&handled);
    let notify_for_producer = Arc::clone(&notify);

    let _saga_proxy = system
        .spawn_saga_with_store(SagaId::new(RECORDING_SAGA_ID), journal_store, move || {
            RecordingSaga {
                handled: Arc::clone(&handled_for_producer),
                notify: Arc::clone(&notify_for_producer),
            }
        })
        .subscribed_to(system.subscription(&order_id))
        .spawn()
        .await;

    // When: the record is written only to the system's default store
    append_order_placed(&default_store, &order_id, 1, "SKU-VIA-DEFAULT").await;

    // Then
    wait_for_handled(&handled, &notify, 1).await;
    assert_eq!(
        handled_skus(&handled),
        vec!["SKU-VIA-DEFAULT".to_owned()],
        "a store-less subscription must deliver records written to the default store"
    );
}

// Override beats default: the saga's own journal

/// A journal named at spawn time must win over the system default — and the
/// default must not receive the saga's stream at all.
#[tokio::test]
async fn spawn_saga_with_store_overrides_the_default_journal_store() {
    // Given
    let ps = ProcessSystem::new().await;
    let default_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let override_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let system = EventSourceSystem::builder(ps)
        .with_codec::<JsonCodec>()
        .with_event_store(Arc::clone(&default_store))
        .build();

    let upstream_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let order_id = AggregateId::new("saga-override-journal-orders");
    let saga_id = SagaId::new(RECORDING_SAGA_ID);

    let handled: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let notify = Arc::new(Notify::new());
    let handled_for_producer = Arc::clone(&handled);
    let notify_for_producer = Arc::clone(&notify);

    // When
    let _saga_proxy = system
        .spawn_saga_with_store(saga_id.clone(), Arc::clone(&override_store), move || {
            RecordingSaga {
                handled: Arc::clone(&handled_for_producer),
                notify: Arc::clone(&notify_for_producer),
            }
        })
        .subscribed_to(Subscription::stream(&upstream_store, &order_id))
        .spawn()
        .await;

    append_order_placed(&upstream_store, &order_id, 1, "SKU-OVERRIDE-JOURNAL").await;
    wait_for_handled(&handled, &notify, 1).await;

    // Then
    assert_eq!(
        wait_for_persisted(&override_store, &saga_id).await,
        vec!["SKU-OVERRIDE-JOURNAL".to_owned()],
        "the explicitly named journal store must receive the saga's own events"
    );
    assert!(
        loaded_stream(&default_store, &saga_id).await.is_empty(),
        "the system default must hold no record of the saga's stream — not its \
         domain events and not its outbox markers — once the spawn names a journal"
    );
}

// Override beats default: the upstream subscription

/// An upstream store named through `Subscription::stream` must win over the
/// system default, even when the default holds a record under the very same
/// stream key.
#[tokio::test]
async fn explicit_subscription_store_overrides_the_default_upstream_store() {
    // Given: the same stream key exists in both stores, holding different records
    let ps = ProcessSystem::new().await;
    let default_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let override_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let system = EventSourceSystem::builder(ps)
        .with_codec::<JsonCodec>()
        .with_event_store(Arc::clone(&default_store))
        .build();

    let order_id = AggregateId::new("saga-override-subscription-orders");
    // Seeded before the spawn so a subscription that wrongly resolved to the
    // default would deliver it on its very first poll.
    append_order_placed(&default_store, &order_id, 1, "SKU-FROM-DEFAULT").await;
    append_order_placed(&override_store, &order_id, 1, "SKU-FROM-OVERRIDE-1").await;

    let handled: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let notify = Arc::new(Notify::new());
    let handled_for_producer = Arc::clone(&handled);
    let notify_for_producer = Arc::clone(&notify);

    // When
    let _saga_proxy = system
        .spawn_saga(SagaId::new(RECORDING_SAGA_ID), move || RecordingSaga {
            handled: Arc::clone(&handled_for_producer),
            notify: Arc::clone(&notify_for_producer),
        })
        .subscribed_to(Subscription::stream(&override_store, &order_id))
        .spawn()
        .await;

    wait_for_handled(&handled, &notify, 1).await;
    // A second record on the overriding store acts as the barrier: once it has
    // been handled the poller has completed a further cycle with the default
    // store's decoy already visible, so "not delivered yet" and "never
    // delivered" are no longer the same observation.
    append_order_placed(&override_store, &order_id, 2, "SKU-FROM-OVERRIDE-2").await;
    wait_for_handled(&handled, &notify, 2).await;

    // Then
    assert_eq!(
        handled_skus(&handled),
        vec![
            "SKU-FROM-OVERRIDE-1".to_owned(),
            "SKU-FROM-OVERRIDE-2".to_owned()
        ],
        "only the explicitly subscribed store may be polled; the default store's \
         record under the same stream key must never reach handle()"
    );
}

// Compile-fail coverage
//
// Calling the store-less `spawn_saga` / `subscription` on a system that was
// never given a default store must be a COMPILE ERROR (the typestate guard).
// An integration test cannot express that, so it is verified via rustdoc
// compile_fail doctests on the store-less saga entry points.
// See: nitinol-saga/src/system_ext.rs
