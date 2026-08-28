//! Replay path — crash-restart re-dispatch via `SagaEffect::tell`.
//!
//! `SagaEffect::tell` serializes the command as JSON and stores the bytes as
//! the crash-restart payload in the `TellRequested` outbox marker.  When the
//! saga process restarts after a full OS-process crash (no `PendingIntents`),
//! a factory registered via `SagaProps::with_crash_restart_factory` can
//! deserialize those bytes and reconstruct the `TellIntent` for re-dispatch.
//!
//! This test seeds a `TellRequested` with JSON crash-restart bytes (matching
//! what `SagaEffect::tell` produces), registers a factory that deserializes
//! and reconstructs the intent, and verifies that `TellAcked` is produced
//! (not `TellFailed`).

#[path = "common/helpers.rs"]
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
    system::EventSourceSystem, Aggregate, Decider, Decision, Event, SequenceCursor,
};
use nitinol_persistence::store::{EventStore, InMemoryEventStore};
use nitinol_persistence::{AppendingEvent, EventType, Family, LoadQuery, LoadedEvent, TypeName};
use nitinol_runtime::ProcessSystem;
use nitinol_saga::{Saga, SagaContext, SagaEffect, SagaId, SagaProps, TellIntent};

// Domain types

#[derive(Clone, Debug, Serialize, Deserialize)]
struct OrderPlaced {
    sku: String,
}

impl Event for OrderPlaced {
    const EVENT_TYPE: EventType = EventType::new(
        Family::new("tell_crash_restart"),
        TypeName::new("OrderPlaced"),
    );
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ReservationRequested {
    sku: String,
}

impl Event for ReservationRequested {
    const EVENT_TYPE: EventType = EventType::new(
        Family::new("tell_crash_restart"),
        TypeName::new("ReservationRequested"),
    );
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
struct Reserved {
    sku: String,
}

impl Event for Reserved {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("tell_crash_restart"), TypeName::new("Reserved"));
}

#[derive(Default)]
struct Inventory;

impl Aggregate for Inventory {
    type Event = Reserved;

    fn apply(&mut self, _event: Reserved) {}
}

/// `Reserve` derives `Clone + Serialize + Deserialize` because `SagaEffect::tell`
/// serializes the command as crash-restart payload into the `TellRequested` marker.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct Reserve {
    sku: String,
}

impl Decider<Reserve> for Inventory {
    type Output = ();
    type Rejection = std::convert::Infallible;

    fn decide(&self, cmd: Reserve) -> Decision<Reserved, (), Self::Rejection> {
        Decision::persist(vec![Reserved { sku: cmd.sku }]).output(())
    }
}

// Inert saga — only the on_start replay path is exercised.

/// Correlation rule of [`InertSaga`]: the single crash-restart process instance
/// each phase spawns against its own freshly seeded store.
const INERT_SAGA_ID: &str = "tell-crash-restart-saga-1";

#[derive(Default)]
struct InertSaga;

// Active saga — calls SagaEffect::tell in handle so the full builder path
// (helper.rs) is exercised.  Used by the payload-format regression test.

/// Correlation rule of [`ActiveSaga`]: the single process instance every
/// `OrderPlaced` on the payload-format upstream belongs to.
const ACTIVE_SAGA_ID: &str = "tell-payload-format-saga-1";

struct ActiveSaga {
    inventory: MockAggregateProxy<Inventory>,
}

#[async_trait]
impl Saga for ActiveSaga {
    type SubscribedEvent = OrderPlaced;
    type Event = ReservationRequested;
    type ScheduledMessage = ();
    type Error = std::convert::Infallible;

    fn correlate(_event: &Self::SubscribedEvent) -> Option<SagaId> {
        Some(SagaId::new(ACTIVE_SAGA_ID))
    }

    fn apply(&mut self, _event: Self::Event) {}

    async fn handle(
        &mut self,
        event: Self::SubscribedEvent,
        _ctx: &mut SagaContext,
    ) -> Result<SagaEffect<Self::Event>, Self::Error> {
        Ok(SagaEffect::tell(
            self.inventory.clone(),
            Reserve { sku: event.sku },
        ))
    }
}

#[async_trait]
impl Saga for InertSaga {
    type SubscribedEvent = OrderPlaced;
    type Event = ReservationRequested;
    type ScheduledMessage = ();
    type Error = std::convert::Infallible;

    fn correlate(_event: &Self::SubscribedEvent) -> Option<SagaId> {
        Some(SagaId::new(INERT_SAGA_ID))
    }

    fn apply(&mut self, _event: Self::Event) {}

    async fn handle(
        &mut self,
        _event: Self::SubscribedEvent,
        _ctx: &mut SagaContext,
    ) -> Result<SagaEffect<Self::Event>, Self::Error> {
        Ok(SagaEffect::None)
    }
}

// Helpers

/// Append an `OrderPlaced` event to an upstream store so a subscribed saga
/// will fire `handle`.
async fn append_order_placed(
    store: &Arc<dyn EventStore>,
    stream_key: &str,
    sequence: u64,
    sku: &str,
) {
    let payload = serde_json::to_vec(&OrderPlaced {
        sku: sku.to_owned(),
    })
    .map(Bytes::from)
    .expect("encode OrderPlaced must succeed");
    append_raw(
        store,
        stream_key,
        sequence,
        EventType::new(
            Family::new("tell_crash_restart"),
            TypeName::new("OrderPlaced"),
        ),
        payload,
    )
    .await;
}

/// Encode a `TellRequested` payload as `SagaEffect::tell` would: prost
/// `tell_id` (field 1) plus the JSON-serialized command as crash-restart
/// bytes (field 2).
fn encode_tell_requested_with_json_cmd<C: Serialize>(tell_id: u64, cmd: &C) -> Bytes {
    let json = serde_json::to_vec(cmd).expect("command serialization must succeed");
    encode_outbox_tell_requested(tell_id, Some(&json))
}

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

// Test

/// Regression: `SagaEffect::tell` serializes the command as
/// JSON crash-restart bytes in the `TellRequested` marker.  After a full
/// OS-process crash (no `PendingIntents`), a factory registered via
/// `SagaProps::with_crash_restart_factory` can deserialize those bytes and
/// re-dispatch the intent.  A successful re-dispatch must produce `TellAcked`
/// and zero `TellFailed`.
#[tokio::test]
async fn saga_tell_crash_restart_bytes_enable_redispatch_via_factory() {
    let mock = MockAggregateProxy::<Inventory>::new();

    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::builder(ps)
        .with_codec::<JsonCodec>()
        .build();

    let saga_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let saga_id = SagaId::new(INERT_SAGA_ID);

    // Seed a TellRequested with tell_id = 1 and JSON crash-restart bytes that
    // match the output of `SagaEffect::tell(target, Reserve { sku: "SKU-CR-1" })`.
    let payload = encode_tell_requested_with_json_cmd(
        1,
        &Reserve {
            sku: "SKU-CR-1".to_owned(),
        },
    );
    append_raw(&saga_store, saga_id.as_str(), 1, OUTBOX_MARKER, payload).await;

    let upstream_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());

    // Factory: deserialize the JSON bytes back into a `Reserve` command and
    // reconstruct the `TellIntent` with the mock target.  This mirrors what
    // the saga owner would register to re-dispatch `SagaEffect::tell` sends.
    let mock_for_factory = mock.clone();
    let factory = move |bytes: &[u8]| -> Option<TellIntent> {
        let cmd: Reserve = serde_json::from_slice(bytes).ok()?;
        Some(TellIntent::new::<Inventory, Reserve, _>(
            mock_for_factory.clone(),
            cmd,
        ))
    };

    // No PendingIntents — simulates a full OS-process crash (in-memory state gone).
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
                "timed out waiting for TellAcked after crash-restart re-dispatch \
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
        "crash-restart re-dispatch of a SagaEffect::tell must produce exactly one TellAcked"
    );
    assert_eq!(
        failed_count, 0,
        "crash-restart re-dispatch must NOT produce TellFailed — the factory \
         successfully deserialized and reconstructed the TellIntent"
    );

    // Verify the mock received the command with the correct payload.
    let captured = mock.drain_captured::<Reserve>();
    assert_eq!(
        captured.len(),
        1,
        "the mock must receive exactly one Reserve command via crash-restart re-dispatch"
    );
    assert_eq!(
        captured[0].sku, "SKU-CR-1",
        "the re-dispatched Reserve must carry the original sku serialized in the crash-restart bytes"
    );
}

/// Regression: verifies that `SagaEffect::tell` (the actual
/// builder path in `helper.rs`) serializes the command as JSON and stores
/// the result as the crash-restart payload in the `TellRequested` marker,
/// then proves that a factory can use those exact bytes for redispatch.
///
/// Two phases:
/// 1. Run a saga whose `handle` returns `SagaEffect::tell(...)`.  After
///    `TellAcked` is observed, load the raw `TellRequested` payload and assert
///    its format: first 8 bytes = big-endian `tell_id`, remaining bytes =
///    `serde_json`-encoded `Reserve`.
/// 2. Seed a fresh saga store with that same `TellRequested` payload, spawn
///    an `InertSaga` with `with_crash_restart_factory`, and assert that
///    `TellAcked` is produced and the mock receives the correct command.
#[tokio::test]
async fn saga_tell_produces_correct_crash_restart_payload_format_and_enables_redispatch() {
    // Phase 1 — run SagaEffect::tell through a real saga handle()
    let mock_p1 = MockAggregateProxy::<Inventory>::new();

    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::builder(ps)
        .with_codec::<JsonCodec>()
        .build();

    let saga_store_p1: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let saga_id_p1 = SagaId::new(ACTIVE_SAGA_ID);

    let upstream_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let upstream_key = "tell-payload-format-upstream";
    append_order_placed(&upstream_store, upstream_key, 1, "SKU-PAYLOAD-CHECK").await;

    let mock_for_saga = mock_p1.clone();
    let _saga_proxy_p1 =
        SagaProps::<ActiveSaga>::new(saga_id_p1.clone(), Arc::clone(&saga_store_p1), move || {
            ActiveSaga {
                inventory: mock_for_saga.clone(),
            }
        })
        .with_codec(system.codec::<ReservationRequested>())
        .with_subscription(
            Arc::clone(&upstream_store),
            system.codec::<OrderPlaced>(),
            SequenceCursor::Stream {
                key: upstream_key.to_owned(),
                after: 0,
            },
        )
        .spawn(system.process_system())
        .await;

    // Wait for TellAcked to confirm that SagaEffect::tell completed its path.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let p1_events = loop {
        let events = load_saga_events(&saga_store_p1, &saga_id_p1).await;
        if events
            .iter()
            .any(|e| matches!(outbox_kind_of(e), Some(OutboxKind::TellAcked(_))))
        {
            break events;
        }
        if std::time::Instant::now() >= deadline {
            let types: Vec<_> = events.iter().map(|e| e.event_type.to_string()).collect();
            panic!(
                "timed out waiting for TellAcked from SagaEffect::tell (events: {:?})",
                types
            );
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    };

    // Verify payload format: prost TellRequested { tell_id, crash_restart }
    // where crash_restart holds the JSON-serialized command.
    let tell_requested_event = p1_events
        .iter()
        .find(|e| matches!(outbox_kind_of(e), Some(OutboxKind::TellRequested(_))))
        .expect("SagaEffect::tell must produce a TellRequested outbox marker");

    let decoded = match common::decode_outbox_kind(&tell_requested_event.payload) {
        OutboxKind::TellRequested(p) => p,
        _ => panic!("expected TellRequested outbox marker, got a different kind"),
    };

    let json_bytes = decoded.crash_restart.unwrap_or_else(|| {
        panic!(
            "TellRequested from SagaEffect::tell must carry crash_restart bytes \
             (field 2 present) — the JSON-encoded command; helper may have regressed \
             to TellIntent::new without a crash-restart payload"
        )
    });
    let decoded_cmd: Reserve = serde_json::from_slice(&json_bytes).unwrap_or_else(|e| {
        panic!(
            "crash_restart bytes in TellRequested payload must be valid JSON for \
             Reserve command; serde_json error: {e} — raw bytes (hex): {:?}",
            json_bytes
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<Vec<_>>()
        )
    });
    assert_eq!(
        decoded_cmd.sku, "SKU-PAYLOAD-CHECK",
        "crash_restart bytes in TellRequested payload must encode the original Reserve \
         command as produced by SagaEffect::tell"
    );

    let raw_payload = &tell_requested_event.payload;

    // Phase 2 — crash-restart redispatch using the real SagaEffect::tell bytes
    let mock_p2 = MockAggregateProxy::<Inventory>::new();
    let saga_store_p2: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let saga_id_p2 = SagaId::new(INERT_SAGA_ID);

    // Seed a fresh saga store with the exact payload that SagaEffect::tell produced.
    // This simulates a crash (in-memory PendingIntents are gone) while the
    // TellRequested marker survives in durable storage.
    append_raw(
        &saga_store_p2,
        saga_id_p2.as_str(),
        1,
        OUTBOX_MARKER,
        raw_payload.clone(),
    )
    .await;

    // Factory: receives the JSON bytes (payload[8..]) and reconstructs Reserve.
    let mock_for_factory = mock_p2.clone();
    let factory = move |bytes: &[u8]| -> Option<TellIntent> {
        let cmd: Reserve = serde_json::from_slice(bytes).ok()?;
        Some(TellIntent::new::<Inventory, Reserve, _>(
            mock_for_factory.clone(),
            cmd,
        ))
    };

    // No PendingIntents — simulates full OS-process crash (in-memory state lost).
    let _saga_proxy_p2 = SagaProps::<InertSaga>::new(
        saga_id_p2.clone(),
        Arc::clone(&saga_store_p2),
        InertSaga::default,
    )
    .with_codec(system.codec::<ReservationRequested>())
    .with_subscription(
        Arc::clone(&upstream_store),
        system.codec::<OrderPlaced>(),
        SequenceCursor::Stream {
            key: "no-such-upstream-p2".to_owned(),
            after: 0,
        },
    )
    .with_crash_restart_factory(factory)
    .spawn(system.process_system())
    .await;

    // Wait for TellAcked to confirm redispatch succeeded.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let p2_events = loop {
        let events = load_saga_events(&saga_store_p2, &saga_id_p2).await;
        if events
            .iter()
            .any(|e| matches!(outbox_kind_of(e), Some(OutboxKind::TellAcked(_))))
        {
            break events;
        }
        if std::time::Instant::now() >= deadline {
            let types: Vec<_> = events.iter().map(|e| e.event_type.to_string()).collect();
            panic!(
                "timed out waiting for TellAcked after crash-restart redispatch \
                 using SagaEffect::tell payload (events: {:?})",
                types
            );
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    };

    let acked_count = p2_events
        .iter()
        .filter(|e| matches!(outbox_kind_of(e), Some(OutboxKind::TellAcked(_))))
        .count();
    let failed_count = p2_events
        .iter()
        .filter(|e| matches!(outbox_kind_of(e), Some(OutboxKind::TellFailed(_))))
        .count();

    assert_eq!(
        acked_count, 1,
        "crash-restart redispatch using SagaEffect::tell payload must produce exactly one TellAcked"
    );
    assert_eq!(
        failed_count, 0,
        "crash-restart redispatch must NOT produce TellFailed — \
         the factory successfully deserialized the SagaEffect::tell crash-restart bytes"
    );

    let captured = mock_p2.drain_captured::<Reserve>();
    assert_eq!(
        captured.len(),
        1,
        "crash-restart redispatch must deliver exactly one Reserve command to the mock"
    );
    assert_eq!(
        captured[0].sku, "SKU-PAYLOAD-CHECK",
        "redispatched Reserve must carry the original sku as encoded by SagaEffect::tell"
    );
}
