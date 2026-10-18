//! Regression test: when the durable `Ended` marker cannot be written, the
//! saga must NOT stop.
//!
//! The End interpreter writes the `Ended` marker before transitioning the
//! saga to a stopped/draining state.  If the append fails without the fix
//! the saga would stop anyway, leaving the stream without the restart guard.
//! The instance the host spawns for the next record of the same correlation id
//! would then replay a stream carrying no `Ended` marker and go on deciding —
//! a saga that believes it terminated, revived.
//!
//! With the fix, a failed `Ended` append returns without stopping — as
//! `InterpretOutcome::AppendFailed`, which reports that an append this input
//! required never landed and is therefore also what keeps the input from being
//! recorded as consumed — so the saga remains alive.  The next incoming
//! upstream event again triggers `handle`, which can return `SagaEffect::End`
//! again, retrying the append.
//!
//! This test verifies the alive-after-failure invariant by giving the system a
//! journal that rejects every append addressed to the saga's own stream, and
//! confirming that two successive upstream events each trigger `Saga::handle`.
//! Only that one stream is refused: a system distributes one journal, so the
//! upstream rows the test seeds are tenants of the very store whose other half
//! is unwritable.

#[path = "common/helpers.rs"]
mod common;
use common::JsonCodec;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use serde::{Deserialize, Serialize};
use tokio::sync::Notify;

use nitinol_eventsource::{system::EventSourceSystem, Event};
use nitinol_persistence::error::{AppendError, LoadError};
use nitinol_persistence::provider::{CustomStore, PersistenceProvider};
use nitinol_persistence::store::{
    EventStore, EventStream, InMemoryEventStore, InMemoryOffsetStore, Journal,
};
use nitinol_persistence::{
    AggregateId, AppendOutcome, AppendingEvent, EventType, Family, LoadQuery, TypeName,
};
use nitinol_runtime::ProcessSystem;
use nitinol_saga::{
    Saga, SagaContext, SagaEffect, SagaId, SagaManagerProps, SagaSubscribe, Subscription,
};

// Domain types

#[derive(Clone, Debug, Serialize, Deserialize)]
struct OrderEvent {
    key: String,
}

impl Event for OrderEvent {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("ended_fail"), TypeName::new("OrderEvent"));
    type Codec = JsonCodec;
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct SagaEvent;

impl Event for SagaEvent {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("ended_fail"), TypeName::new("SagaEvent"));
    type Codec = JsonCodec;
}

// Saga: always returns End; counts handle invocations

struct AlwaysEndSaga {
    handle_count: Arc<AtomicUsize>,
    handled: Arc<Notify>,
}

/// Correlation rule of [`AlwaysEndSaga`]: both seeded upstream events name the
/// same process instance, so both must reach `handle`.
const ALWAYS_END_SAGA_ID: &str = "ended-fail-saga";

/// The whole alphabet this saga reacts to: one observed source, no deadline.
// The observed payload is decoded and delivered as the variant it belongs to;
// what this saga decides does not read it, which `dead_code` cannot tell apart
// from a field nothing fills.
#[allow(dead_code)]
#[derive(SagaSubscribe)]
enum Observed {
    Order(OrderEvent),
}

#[async_trait]
impl Saga for AlwaysEndSaga {
    type Subscribe = Observed;
    type Event = SagaEvent;
    type Error = std::convert::Infallible;

    fn correlate(_input: &Self::Subscribe) -> Option<SagaId> {
        Some(SagaId::new(ALWAYS_END_SAGA_ID))
    }

    async fn handle(
        &mut self,
        _input: Self::Subscribe,
        _ctx: &mut SagaContext,
    ) -> Result<SagaEffect<SagaEvent>, Self::Error> {
        self.handle_count.fetch_add(1, Ordering::SeqCst);
        self.handled.notify_one();
        Ok(SagaEffect::End)
    }
}

// Journal: rejects every append to one named stream, and passes everything
// else to the in-memory store it wraps. The saga therefore can never write the
// Ended marker (or any other event) onto its own stream, while the upstream
// rows it reacts to are still writable and readable.

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
            return Err(AppendError::Backend("always fails".into()));
        }
        self.inner.append(key, events).await
    }

    async fn load(&self, query: LoadQuery) -> Result<EventStream<'_>, LoadError> {
        self.inner.load(query).await
    }
}

// Helpers

async fn append_order(store: &Journal, agg_id: &AggregateId, sequence: u64, key: &str) {
    let payload = serde_json::to_vec(&OrderEvent {
        key: key.to_owned(),
    })
    .map(Bytes::from)
    .expect("encode OrderEvent");
    store
        .append(
            agg_id.as_str(),
            vec![AppendingEvent {
                sequence,
                event_type: OrderEvent::EVENT_TYPE,
                payload,
                occurred_at: jiff::Timestamp::now(),
            }],
        )
        .await
        .expect("append OrderEvent to upstream store must succeed");
}

// Test

/// When the `Ended` marker append fails, the saga must stay alive and handle
/// subsequent upstream events rather than silently stopping without the
/// restart guard.
#[tokio::test]
async fn saga_stays_alive_when_ended_append_fails() {
    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::builder(ps)
        .with_persistence(PersistenceProvider::new(
            CustomStore::builder()
                .eventstore(AppendRefusingStore::refusing(ALWAYS_END_SAGA_ID))
                .saga_offsets(InMemoryOffsetStore::default())
                .build(),
        ))
        .build();

    let order_id = AggregateId::new("ended-fail-order");

    // Pre-seed two upstream events so both arrive before the saga starts.
    let seeding = system.persistence().event_store();
    append_order(&seeding, &order_id, 1, "first").await;
    append_order(&seeding, &order_id, 2, "second").await;

    let handle_count = Arc::new(AtomicUsize::new(0));
    let handled = Arc::new(Notify::new());

    let handle_count_clone = Arc::clone(&handle_count);
    let handled_clone = Arc::clone(&handled);

    let _host = SagaManagerProps::<AlwaysEndSaga>::new(move |_components| AlwaysEndSaga {
        handle_count: Arc::clone(&handle_count_clone),
        handled: Arc::clone(&handled_clone),
    })
    .observing::<OrderEvent>(Subscription::stream(&order_id))
    .spawn(&system)
    .await;

    // Wait for at least one handle call to confirm event delivery works.
    tokio::time::timeout(Duration::from_secs(3), handled.notified())
        .await
        .expect("the first upstream event must be delivered to the saga within 3 s");

    // Give the saga time to process both events.
    tokio::time::sleep(Duration::from_millis(500)).await;

    let count = handle_count.load(Ordering::SeqCst);
    assert!(
        count >= 2,
        "the saga must handle both upstream events (proving it did not stop after \
         the failed Ended append); actual handle_count = {count}"
    );
}
