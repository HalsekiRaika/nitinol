//! Spec C-9 / replay path — **crash-restart re-dispatch**: when the saga
//! process starts after a full OS-process crash (no in-memory
//! `PendingIntents`), the replay path must re-dispatch pending tells using
//! the crash-restart factory registered via
//! `SagaProps::with_crash_restart_factory`, provided that the `TellRequested`
//! marker carries crash-restart bytes.

mod common;
use common::{encode_outbox_tell_requested, outbox_kind_of, JsonCodec, OutboxKind, OUTBOX_MARKER};

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use futures_util::TryStreamExt;
use serde::{Deserialize, Serialize};

use nitinol_eventsource::test_helpers::MockAggregateProxy;
use nitinol_eventsource::{
    system::EventSourceSystem, Aggregate, Context, Decider, Effect, Event, SequenceCursor,
};
use nitinol_persistence::store::{EventStore, InMemoryEventStore};
use nitinol_persistence::{AppendingEvent, EventType, Family, LoadQuery, LoadedEvent, TypeName};
use nitinol_runtime::ProcessSystem;
use nitinol_saga::{Saga, SagaContext, SagaEffect, SagaId, SagaProps, TellIntent};

// ---------------------------------------------------------------------------
// Domain types — minimal; Reserve is a unit struct so the crash-restart
// bytes can be anything (the factory ignores them and always returns Reserve).
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Serialize, Deserialize)]
struct OrderPlaced {
    sku: String,
}

impl Event for OrderPlaced {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("crash_restart"), TypeName::new("OrderPlaced"));
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ReservationRequested {
    sku: String,
}

impl Event for ReservationRequested {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("crash_restart"), TypeName::new("ReservationRequested"));
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
struct Reserved {
    sku: String,
}

impl Event for Reserved {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("crash_restart"), TypeName::new("Reserved"));
}

#[derive(Default)]
struct Inventory;

impl Aggregate for Inventory {
    type Event = Reserved;

    fn apply(&mut self, _event: Reserved) {}
}

/// Unit-struct command — no payload to serialize, so the crash-restart bytes
/// are a fixed marker (`b"Reserve"`); the factory ignores them and always
/// reconstructs the intent.
#[derive(Clone, Debug, PartialEq)]
struct Reserve;

#[async_trait]
impl Decider<Reserve> for Inventory {
    type Rejection = std::convert::Infallible;

    async fn decide(
        &self,
        _cmd: Reserve,
        _ctx: &mut Context,
    ) -> Result<Effect<Reserved>, Self::Rejection> {
        Ok(Effect::persist(Reserved {
            sku: "crash-restart-sku".to_owned(),
        }))
    }
}

// ---------------------------------------------------------------------------
// Inert saga — only the on_start replay path is exercised.
// ---------------------------------------------------------------------------

#[derive(Default)]
struct InertSaga;

#[async_trait]
impl Saga for InertSaga {
    type SubscribedEvent = OrderPlaced;
    type Event = ReservationRequested;
    type State = ();
    type Error = std::convert::Infallible;

    fn apply(&mut self, _event: Self::Event) {}

    async fn handle(
        &mut self,
        _event: Self::SubscribedEvent,
        _ctx: &mut SagaContext,
    ) -> Result<SagaEffect<Self::Event>, Self::Error> {
        Ok(SagaEffect::None)
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

async fn append_raw(
    store: &Arc<dyn EventStore>,
    stream_key: &str,
    sequence: u64,
    event_type: EventType,
    payload: Bytes,
) {
    store
        .append(
            stream_key,
            vec![AppendingEvent {
                sequence,
                event_type,
                payload,
                occurred_at: jiff::Timestamp::now(),
            }],
        )
        .await
        .expect("append must succeed");
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

// ---------------------------------------------------------------------------
// Test
// ---------------------------------------------------------------------------

/// When the saga restarts after a full OS-process crash (no `PendingIntents`),
/// and the `TellRequested` marker carries crash-restart bytes, and a factory
/// was registered via `SagaProps::with_crash_restart_factory`, the replay path
/// must reconstruct the `TellIntent` via the factory and dispatch it to the
/// retry executor.  A successful dispatch must produce exactly one `TellAcked`
/// and zero `TellFailed`.
#[tokio::test]
async fn crash_restart_factory_redispatches_unacked_tell_and_produces_tell_acked() {
    let mock = MockAggregateProxy::<Inventory>::new();

    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::new(ps).with_codec::<JsonCodec>().build();

    let saga_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let saga_id = SagaId::new("crash-restart-saga-1");

    // Seed a TellRequested with tell_id = 1 and crash-restart bytes = b"Reserve".
    // The factory below identifies "Reserve" intents by these bytes.
    let payload = encode_outbox_tell_requested(1, Some(b"Reserve"));
    append_raw(&saga_store, saga_id.as_str(), 1, OUTBOX_MARKER, payload).await;

    let upstream_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());

    let routed = saga_id.clone();
    let route_fn = move |_event: &OrderPlaced| -> Option<SagaId> { Some(routed.clone()) };

    // Crash-restart factory: ignore the bytes (Reserve is unit struct), always
    // return a fresh TellIntent that tells Inventory to Reserve.
    let mock_for_factory = mock.clone();
    let factory = move |_bytes: &[u8]| -> Option<TellIntent> {
        Some(TellIntent::new::<Inventory, Reserve, _>(
            mock_for_factory.clone(),
            Reserve,
        ))
    };

    // No PendingIntents provided — the default is a fresh empty registry,
    // which simulates a crash restart (in-memory state is gone).
    let _saga_proxy =
        SagaProps::<InertSaga>::new(saga_id.clone(), Arc::clone(&saga_store), InertSaga::default)
            .with_codec(system.codec::<ReservationRequested>())
            .with_subscription(
                Arc::clone(&upstream_store),
                system.codec::<OrderPlaced>(),
                SequenceCursor::Stream {
                    key: "no-such-stream".to_owned(),
                    after: 0,
                },
                route_fn,
            )
            .with_crash_restart_factory(factory)
            .spawn(system.process_system())
            .await;

    // Wait for the crash-restart re-dispatch to produce TellAcked.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let events = loop {
        let events = load_saga_events(&saga_store, &saga_id).await;
        let acked_count = events
            .iter()
            .filter(|e| matches!(outbox_kind_of(e), Some(OutboxKind::TellAcked(_))))
            .count();
        if acked_count >= 1 {
            break events;
        }
        if std::time::Instant::now() >= deadline {
            let event_types: Vec<_> = events.iter().map(|e| e.event_type.to_string()).collect();
            panic!(
                "timed out waiting for TellAcked on crash-restart re-dispatch \
                 (event_types: {:?})",
                event_types
            );
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    };

    let acked_count = events
        .iter()
        .filter(|e| matches!(outbox_kind_of(e), Some(OutboxKind::TellAcked(_))))
        .count();
    let failed_count = events
        .iter()
        .filter(|e| matches!(outbox_kind_of(e), Some(OutboxKind::TellFailed(_))))
        .count();

    assert_eq!(
        acked_count, 1,
        "crash-restart re-dispatch must produce exactly one TellAcked"
    );
    assert_eq!(
        failed_count, 0,
        "crash-restart re-dispatch must NOT produce TellFailed — the factory \
         successfully reconstructed the intent"
    );
}
