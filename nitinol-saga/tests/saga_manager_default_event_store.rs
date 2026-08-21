//! The saga manager entry point on a system that already holds a default
//! [`EventStore`].
//!
//! `system.saga_manager_props(subscription, producer)` returns a manager
//! builder with everything the system already knows filled in: the store each
//! instance journals onto, the codec for the instances' own events, and the
//! codec for the subscribed ones.  Only the subscription stays in the caller's
//! hands, so it is also the manager's override point —
//! `Subscription::stream(&store, &key)` names an upstream store explicitly
//! where `system.subscription(&key)` takes the default.
//!
//! The store is stream-keyed, so one default store holds the upstream stream
//! and every correlated instance's own stream side by side; that is what the
//! first test observes, on the id `Saga::correlate` derived rather than on a
//! fixed one.

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
    order: String,
}

impl Event for OrderPlaced {
    const EVENT_TYPE: EventType = EventType::new(
        Family::new("saga.manager.default.store"),
        TypeName::new("OrderPlaced"),
    );
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ReservationRequested {
    order: String,
}

impl Event for ReservationRequested {
    const EVENT_TYPE: EventType = EventType::new(
        Family::new("saga.manager.default.store"),
        TypeName::new("ReservationRequested"),
    );
}

/// Prefix of every instance id [`FanoutSaga::correlate`] derives; the order key
/// is appended verbatim, so the journal assertion observes an id the manager
/// computed rather than one the test fixed in advance.
const INSTANCE_PREFIX: &str = "mgr-default-store-";

struct FanoutSaga {
    handled: Arc<Mutex<Vec<String>>>,
    notify: Arc<Notify>,
}

#[async_trait]
impl Saga for FanoutSaga {
    type SubscribedEvent = OrderPlaced;
    type Event = ReservationRequested;
    type ScheduledMessage = ();
    type Error = std::convert::Infallible;

    fn correlate(event: &Self::SubscribedEvent) -> Option<SagaId> {
        Some(SagaId::new(format!("{INSTANCE_PREFIX}{}", event.order)))
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
            .push(event.order.clone());
        self.notify.notify_one();
        Ok(SagaEffect::persist(ReservationRequested {
            order: event.order,
        }))
    }
}

// Helpers

async fn append_order_placed(
    store: &Arc<dyn EventStore>,
    key: &AggregateId,
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

fn handled_orders(handled: &Arc<Mutex<Vec<String>>>) -> Vec<String> {
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
            if handled_orders(handled).len() >= expected {
                return;
            }
            notified.await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "timed out waiting for {expected} handle() calls (got {:?})",
            handled_orders(handled)
        )
    });
}

/// Orders of the instance's own events on its own stream in `store`.
async fn instance_persisted_orders(store: &Arc<dyn EventStore>, saga_id: &SagaId) -> Vec<String> {
    let loaded: Vec<LoadedEvent> = store
        .load(LoadQuery::by_stream(saga_id))
        .await
        .expect("instance stream load must succeed")
        .try_collect()
        .await
        .expect("collect instance events must succeed");
    loaded
        .iter()
        .filter(|event| event.event_type == ReservationRequested::EVENT_TYPE)
        .map(|event| {
            serde_json::from_slice::<ReservationRequested>(&event.payload)
                .expect("the instance's own event must be encoded with the system codec")
                .order
        })
        .collect()
}

async fn wait_for_instance_persisted(store: &Arc<dyn EventStore>, saga_id: &SagaId) -> Vec<String> {
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    loop {
        let orders = instance_persisted_orders(store, saga_id).await;
        if !orders.is_empty() {
            return orders;
        }
        if std::time::Instant::now() >= deadline {
            panic!("timed out waiting for the manager's instance to persist its own event");
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

// Default store: both directions of the manager

/// `system.saga_manager_props(system.subscription(&key), producer)` must resolve
/// both stores from the system default: the upstream it polls and the journal
/// each correlated instance writes to.
#[tokio::test]
async fn store_less_manager_props_uses_the_system_default_store_for_upstream_and_journal() {
    // Given: one default store, holding the upstream stream
    let ps = ProcessSystem::new().await;
    let default_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let system = EventSourceSystem::new(ps)
        .with_codec::<JsonCodec>()
        .with_event_store(Arc::clone(&default_store))
        .build();

    let upstream_key = AggregateId::new("mgr-default-store-orders");

    let handled: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let notify = Arc::new(Notify::new());
    let handled_for_producer = Arc::clone(&handled);
    let notify_for_producer = Arc::clone(&notify);

    // When: neither store is named at the call site
    let _manager_proxy = system
        .saga_manager_props(system.subscription(&upstream_key), move || FanoutSaga {
            handled: Arc::clone(&handled_for_producer),
            notify: Arc::clone(&notify_for_producer),
        })
        .spawn(system.process_system())
        .await;

    append_order_placed(&default_store, &upstream_key, 1, "A").await;
    wait_for_handled(&handled, &notify, 1).await;

    // Then: the upstream came from the default store ...
    assert_eq!(
        handled_orders(&handled),
        vec!["A".to_owned()],
        "a store-less manager subscription must deliver records written to the default store"
    );
    // ... and the instance journalled back onto it, under the correlated id
    assert_eq!(
        wait_for_instance_persisted(&default_store, &SagaId::new(format!("{INSTANCE_PREFIX}A")))
            .await,
        vec!["A".to_owned()],
        "each instance the manager spawns must journal onto the default store, \
         keyed by the id Saga::correlate named"
    );
}

// Override beats default: the manager's upstream

/// An upstream store named through `Subscription::stream` must win over the
/// system default, even when the default holds a record under the very same
/// stream key.
#[tokio::test]
async fn explicit_subscription_store_overrides_the_default_upstream_for_the_manager() {
    // Given: the same stream key exists in both stores, holding different records
    let ps = ProcessSystem::new().await;
    let default_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let override_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let system = EventSourceSystem::new(ps)
        .with_codec::<JsonCodec>()
        .with_event_store(Arc::clone(&default_store))
        .build();

    let upstream_key = AggregateId::new("mgr-override-store-orders");
    // Seeded before the spawn so a subscription that wrongly resolved to the
    // default would deliver it on the manager's very first poll.
    append_order_placed(&default_store, &upstream_key, 1, "FROM-DEFAULT").await;
    append_order_placed(&override_store, &upstream_key, 1, "FROM-OVERRIDE-1").await;

    let handled: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let notify = Arc::new(Notify::new());
    let handled_for_producer = Arc::clone(&handled);
    let notify_for_producer = Arc::clone(&notify);

    // When
    let _manager_proxy = system
        .saga_manager_props(
            Subscription::stream(&override_store, &upstream_key),
            move || FanoutSaga {
                handled: Arc::clone(&handled_for_producer),
                notify: Arc::clone(&notify_for_producer),
            },
        )
        .spawn(system.process_system())
        .await;

    wait_for_handled(&handled, &notify, 1).await;
    // A second record on the overriding store acts as the barrier: once it has
    // been handled the manager has completed a further poll cycle with the
    // default store's decoy already visible, so "not delivered yet" and "never
    // delivered" are no longer the same observation.
    append_order_placed(&override_store, &upstream_key, 2, "FROM-OVERRIDE-2").await;
    wait_for_handled(&handled, &notify, 2).await;

    // Then
    assert_eq!(
        handled_orders(&handled),
        vec!["FROM-OVERRIDE-1".to_owned(), "FROM-OVERRIDE-2".to_owned()],
        "only the explicitly subscribed store may be polled; the default store's \
         record under the same stream key must never reach an instance"
    );
}

// Compile-fail coverage
//
// Calling the store-less `saga_manager_props` / `subscription` on a system that
// was never given a default store must be a COMPILE ERROR (the typestate
// guard).  An integration test cannot express that, so it is verified via
// rustdoc compile_fail doctests on the store-less saga entry points.
// See: nitinol-saga/src/system_ext.rs
