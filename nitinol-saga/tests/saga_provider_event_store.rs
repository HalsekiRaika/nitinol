//! A saga hosted from a system, and which store each of its two directions
//! resolves.
//!
//! A saga touches two directions of one store: the journal it writes its own
//! events onto, and the journal it polls for upstream records.  Neither is
//! named at the wiring — both are the store the provider the system was built
//! with supplies — and that holds through either entry point:
//! `system.host::<S>(producer).spawn()` and
//! `SagaManagerProps::<S>::new(producer).spawn(&system)`.
//!
//! Reaching a different destination is written down rather than defaulted, and
//! what writes it down is a second system: a saga hosted on one system reaches
//! no other system's journal in either direction.  There is no per-spawn
//! override, and no store on the props or the subscription to name one with.
//!
//! The two directions are pinned separately: a wiring that resolves the journal
//! from the provider but the upstream from somewhere else (or the reverse) still
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
use nitinol_persistence::provider::{CustomStore, PersistenceProvider};
use nitinol_persistence::store::{InMemoryEventStore, InMemoryOffsetStore, Journal};
use nitinol_persistence::{
    AggregateId, AppendingEvent, EventType, Family, LoadQuery, LoadedEvent, TypeName,
};
use nitinol_runtime::ProcessSystem;
use nitinol_saga::{
    Saga, SagaContext, SagaEffect, SagaId, SagaManagerProps, SagaSubscribe, SagaSystemExt,
    Subscription,
};

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
    type Codec = JsonCodec;
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
    type Codec = JsonCodec;
}

/// Correlation rule of [`RecordingSaga`]: every order belongs to the single
/// reservation process each test in this file spawns.
const RECORDING_SAGA_ID: &str = "saga-default-store-reservation";

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

// Helpers

async fn append_order_placed(store: &Journal, key: &AggregateId, sequence: u64, sku: &str) {
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

async fn loaded_stream(store: &Journal, key: &SagaId) -> Vec<LoadedEvent> {
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
/// proxy a spawn returns looks the same whichever store its instances write to.
async fn persisted_skus(store: &Journal, saga_id: &SagaId) -> Vec<String> {
    loaded_stream(store, saga_id)
        .await
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

// The provider's journal: the saga's own stream

/// `host::<S>(producer)` — no journal argument — must write the saga's own
/// events onto the journal the system's provider supplies.
#[tokio::test]
async fn host_journals_onto_the_providers_event_store() {
    // Given: a system over one provider, whose journal holds the upstream the
    // saga observes beside the saga's own stream
    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::builder(ps)
        .with_persistence(PersistenceProvider::new(
            CustomStore::builder()
                .eventstore(InMemoryEventStore::default())
                .saga_offsets(InMemoryOffsetStore::default())
                .build(),
        ))
        .build();
    let default_store = system.persistence().event_store();

    let order_id = AggregateId::new("saga-default-journal-orders");
    let saga_id = SagaId::new(RECORDING_SAGA_ID);

    let handled: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let notify = Arc::new(Notify::new());
    let handled_for_producer = Arc::clone(&handled);
    let notify_for_producer = Arc::clone(&notify);

    // When: hosting without naming a journal store
    let _host_proxy = system
        .host::<RecordingSaga>(move |_components| RecordingSaga {
            handled: Arc::clone(&handled_for_producer),
            notify: Arc::clone(&notify_for_producer),
        })
        .observing::<OrderPlaced>(Subscription::stream(&order_id))
        .spawn()
        .await;

    append_order_placed(&default_store, &order_id, 1, "SKU-DEFAULT-JOURNAL").await;
    wait_for_handled(&handled, &notify, 1).await;

    // Then
    assert_eq!(
        wait_for_persisted(&default_store, &saga_id).await,
        vec!["SKU-DEFAULT-JOURNAL".to_owned()],
        "host must journal onto the store the provider supplies"
    );
}

// The provider's journal: the upstream subscription

/// A `Subscription` names a stream key and nothing else, so the store it is
/// read from must be the one the system's provider supplies — which is what
/// lets a saga fed by an aggregate living in that same store need nothing at
/// the call site.
///
/// Named on the props-first entry point, because that is the form that used to
/// be able to disagree with the system about where the upstream was.
#[tokio::test]
async fn a_subscription_polls_the_providers_event_store() {
    // Given: a system over one provider
    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::builder(ps)
        .with_persistence(PersistenceProvider::new(
            CustomStore::builder()
                .eventstore(InMemoryEventStore::default())
                .saga_offsets(InMemoryOffsetStore::default())
                .build(),
        ))
        .build();
    let default_store = system.persistence().event_store();

    let order_id = AggregateId::new("saga-default-subscription-orders");

    let handled: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let notify = Arc::new(Notify::new());
    let handled_for_producer = Arc::clone(&handled);
    let notify_for_producer = Arc::clone(&notify);

    let _host_proxy = SagaManagerProps::<RecordingSaga>::new(move |_components| RecordingSaga {
        handled: Arc::clone(&handled_for_producer),
        notify: Arc::clone(&notify_for_producer),
    })
    .observing::<OrderPlaced>(Subscription::stream(&order_id))
    .spawn(&system)
    .await;

    // When: the record is written only to the store the provider supplies
    append_order_placed(&default_store, &order_id, 1, "SKU-VIA-DEFAULT").await;

    // Then
    wait_for_handled(&handled, &notify, 1).await;
    assert_eq!(
        handled_skus(&handled),
        vec!["SKU-VIA-DEFAULT".to_owned()],
        "a subscription taken from the system must deliver records written to the \
         store its provider supplies"
    );
}

// A second system: its store receives nothing

/// The journal of the system a saga is spawned on must be the only store its
/// own stream reaches — another system's journal must not receive it at all.
///
/// This is how a saga is placed in another destination now that no store is
/// nameable at the wiring: a second system over the supplier that holds it, and
/// building that second system is the whole of what makes it explicit.
#[tokio::test]
async fn a_saga_spawned_on_one_system_keeps_its_stream_out_of_another_systems_store() {
    // Given: two systems, each over a provider of its own
    let hosting = EventSourceSystem::builder(ProcessSystem::new().await)
        .with_persistence(PersistenceProvider::new(
            CustomStore::builder()
                .eventstore(InMemoryEventStore::default())
                .saga_offsets(InMemoryOffsetStore::default())
                .build(),
        ))
        .build();
    let other = EventSourceSystem::builder(ProcessSystem::new().await)
        .with_persistence(PersistenceProvider::new(
            CustomStore::builder()
                .eventstore(InMemoryEventStore::default())
                .saga_offsets(InMemoryOffsetStore::default())
                .build(),
        ))
        .build();
    let hosting_store = hosting.persistence().event_store();
    let other_store = other.persistence().event_store();

    let order_id = AggregateId::new("saga-override-journal-orders");
    let saga_id = SagaId::new(RECORDING_SAGA_ID);

    let handled: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let notify = Arc::new(Notify::new());
    let handled_for_producer = Arc::clone(&handled);
    let notify_for_producer = Arc::clone(&notify);

    // When
    let _host_proxy = SagaManagerProps::<RecordingSaga>::new(move |_components| RecordingSaga {
        handled: Arc::clone(&handled_for_producer),
        notify: Arc::clone(&notify_for_producer),
    })
    .observing::<OrderPlaced>(Subscription::stream(&order_id))
    .spawn(&hosting)
    .await;

    append_order_placed(&hosting_store, &order_id, 1, "SKU-OVERRIDE-JOURNAL").await;
    wait_for_handled(&handled, &notify, 1).await;

    // Then
    assert_eq!(
        wait_for_persisted(&hosting_store, &saga_id).await,
        vec!["SKU-OVERRIDE-JOURNAL".to_owned()],
        "the journal of the system the saga was spawned on must receive the \
         saga's own events"
    );
    assert!(
        loaded_stream(&other_store, &saga_id).await.is_empty(),
        "another system's store must hold no record of the saga's stream \
         — not its domain events and not its outbox markers — because every \
         store a saga touches comes from the system it was spawned on"
    );
}

// A second system's upstream: it is not polled

/// The journal of the system a saga is hosted on must be the only one polled,
/// even when another system's journal holds a record under the very same stream
/// key.
#[tokio::test]
async fn the_hosting_systems_journal_is_the_only_upstream_polled() {
    // Given: the same stream key exists in both systems' journals, holding
    // different records
    let hosting = EventSourceSystem::builder(ProcessSystem::new().await)
        .with_persistence(PersistenceProvider::new(
            CustomStore::builder()
                .eventstore(InMemoryEventStore::default())
                .saga_offsets(InMemoryOffsetStore::default())
                .build(),
        ))
        .build();
    let other = EventSourceSystem::builder(ProcessSystem::new().await)
        .with_persistence(PersistenceProvider::new(
            CustomStore::builder()
                .eventstore(InMemoryEventStore::default())
                .saga_offsets(InMemoryOffsetStore::default())
                .build(),
        ))
        .build();
    let hosting_store = hosting.persistence().event_store();
    let other_store = other.persistence().event_store();

    let order_id = AggregateId::new("saga-override-subscription-orders");
    // Seeded before the spawn so a subscription that wrongly resolved to the
    // other system's journal would deliver it on its very first poll.
    append_order_placed(&other_store, &order_id, 1, "SKU-FROM-DEFAULT").await;
    append_order_placed(&hosting_store, &order_id, 1, "SKU-FROM-OVERRIDE-1").await;

    let handled: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let notify = Arc::new(Notify::new());
    let handled_for_producer = Arc::clone(&handled);
    let notify_for_producer = Arc::clone(&notify);

    // When
    let _host_proxy = hosting
        .host::<RecordingSaga>(move |_components| RecordingSaga {
            handled: Arc::clone(&handled_for_producer),
            notify: Arc::clone(&notify_for_producer),
        })
        .observing::<OrderPlaced>(Subscription::stream(&order_id))
        .spawn()
        .await;

    wait_for_handled(&handled, &notify, 1).await;
    // A second record on the hosting system's journal acts as the barrier: once
    // it has been handled the poller has completed a further cycle with the
    // other system's decoy already visible, so "not delivered yet" and "never
    // delivered" are no longer the same observation.
    append_order_placed(&hosting_store, &order_id, 2, "SKU-FROM-OVERRIDE-2").await;
    wait_for_handled(&handled, &notify, 2).await;

    // Then
    assert_eq!(
        handled_skus(&handled),
        vec![
            "SKU-FROM-OVERRIDE-1".to_owned(),
            "SKU-FROM-OVERRIDE-2".to_owned()
        ],
        "only the journal of the system the saga is hosted on may be polled; \
         another system's record under the same stream key must never reach \
         handle()"
    );
}

// Compile-fail coverage
//
// Hosting a saga on a provider with no saga-offset slot must be a COMPILE
// ERROR — the delivery host `host` starts has nowhere to record its progress —
// and so must building a system with no provider at all. An integration test
// cannot express a call that must not compile, so both are verified via rustdoc
// compile_fail doctests on the entry points.
// See: nitinol-saga/src/system_ext.rs, nitinol-eventsource/src/system.rs
