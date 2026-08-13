//! Regression test: when the durable `Ended` marker cannot be written, the
//! saga must NOT stop.
//!
//! The End interpreter writes the `Ended` marker before transitioning the
//! saga to a stopped/draining state.  If the append fails without the fix
//! the saga would stop anyway, leaving the stream without the restart guard.
//! The next spawn would then see no `Ended` marker and revive the saga.
//!
//! With the fix, a failed `Ended` append returns `InterpretOutcome::Continue`
//! so the saga remains alive.  The next incoming upstream event again triggers
//! `handle`, which can return `SagaEffect::End` again, retrying the append.
//!
//! This test verifies the alive-after-failure invariant by injecting a saga
//! store that always rejects appends and confirming that two successive
//! upstream events each trigger `Saga::handle`.

#[path = "common/helpers.rs"]
mod common;
use common::JsonCodec;

use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use futures_core::Stream;
use serde::{Deserialize, Serialize};
use tokio::sync::Notify;

use nitinol_eventsource::{system::EventSourceSystem, Event, SequenceCursor};
use nitinol_persistence::error::{AppendError, LoadError};
use nitinol_persistence::store::{EventStore, EventStream, InMemoryEventStore};
use nitinol_persistence::{
    AggregateId, AppendOutcome, AppendingEvent, EventType, Family, LoadQuery, TypeName,
};
use nitinol_runtime::ProcessSystem;
use nitinol_saga::{Saga, SagaContext, SagaEffect, SagaId, SagaProps};

// Domain types

#[derive(Clone, Debug, Serialize, Deserialize)]
struct OrderEvent {
    key: String,
}

impl Event for OrderEvent {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("ended_fail"), TypeName::new("OrderEvent"));
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct SagaEvent;

impl Event for SagaEvent {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("ended_fail"), TypeName::new("SagaEvent"));
}

// Saga: always returns End; counts handle invocations

struct AlwaysEndSaga {
    handle_count: Arc<AtomicUsize>,
    handled: Arc<Notify>,
}

#[async_trait]
impl Saga for AlwaysEndSaga {
    type SubscribedEvent = OrderEvent;
    type Event = SagaEvent;
    type ScheduledMessage = ();
    type Error = std::convert::Infallible;

    fn apply(&mut self, _event: SagaEvent) {}

    async fn handle(
        &mut self,
        _event: OrderEvent,
        _ctx: &mut SagaContext,
    ) -> Result<SagaEffect<SagaEvent>, Self::Error> {
        self.handle_count.fetch_add(1, Ordering::SeqCst);
        self.handled.notify_one();
        Ok(SagaEffect::End)
    }
}

// Saga store: always rejects appends; loads return an empty stream.
// The saga therefore can never write the Ended marker (or any other event).

struct AlwaysFailOnAppendStore;

#[async_trait]
impl EventStore for AlwaysFailOnAppendStore {
    async fn append(
        &self,
        _key: &str,
        _events: Vec<AppendingEvent>,
    ) -> Result<AppendOutcome, AppendError> {
        Err(AppendError::Backend("always fails".into()))
    }

    async fn load(&self, _query: LoadQuery) -> Result<EventStream<'_>, LoadError> {
        let stream: Pin<
            Box<dyn Stream<Item = Result<nitinol_persistence::LoadedEvent, LoadError>> + Send + '_>,
        > = Box::pin(futures_util::stream::empty());
        Ok(stream)
    }
}

// Helpers

async fn append_order(store: &Arc<dyn EventStore>, agg_id: &AggregateId, sequence: u64, key: &str) {
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
    let system = EventSourceSystem::new(ps).with_codec::<JsonCodec>().build();

    let upstream_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let order_id = AggregateId::new("ended-fail-order");

    // Pre-seed two upstream events so both arrive before the saga starts.
    append_order(&upstream_store, &order_id, 1, "first").await;
    append_order(&upstream_store, &order_id, 2, "second").await;

    let saga_store: Arc<dyn EventStore> = Arc::new(AlwaysFailOnAppendStore);
    let saga_id = SagaId::new("ended-fail-saga");

    let handle_count = Arc::new(AtomicUsize::new(0));
    let handled = Arc::new(Notify::new());

    let handle_count_clone = Arc::clone(&handle_count);
    let handled_clone = Arc::clone(&handled);
    let routed = saga_id.clone();

    let _saga_proxy =
        SagaProps::<AlwaysEndSaga>::new(saga_id.clone(), Arc::clone(&saga_store), move || {
            AlwaysEndSaga {
                handle_count: Arc::clone(&handle_count_clone),
                handled: Arc::clone(&handled_clone),
            }
        })
        .with_codec(system.codec::<SagaEvent>())
        .with_subscription(
            Arc::clone(&upstream_store),
            system.codec::<OrderEvent>(),
            SequenceCursor::Stream {
                key: order_id.as_str().to_owned(),
                after: 0,
            },
            move |_event: &OrderEvent| Some(routed.clone()),
        )
        .spawn(system.process_system())
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
