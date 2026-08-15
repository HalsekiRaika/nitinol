//! Replay path — synthetic `TellFailed` and negative cases.
//!
//! ## Positive case
//!
//! When a `TellRequested` is found on restart with **no** crash-restart bytes
//! (i.e. written via [`nitinol_saga::TellIntent::new`] directly) and the
//! process started fresh (no `PendingIntents`, no crash-restart factory), the
//! replay path must append a synthetic `TellFailed` to bring the outbox to a
//! consistent terminal state.
//!
//! Note: `SagaEffect::tell` now always serializes the command as crash-restart
//! bytes.  The "no crash-restart bytes" scenario tested here corresponds to
//! `TellIntent::new` direct usage, or streams written before this feature.
//!
//! ## Negative case
//!
//! When both `TellRequested` and `TellAcked` are already in the saga stream
//! before restart, the replay path must not append any extra outbox marker.
//!
//! For the **supervised-restart re-dispatch** path (same OS process,
//! `PendingIntents` still populated), see the `#[cfg(test)]` module in
//! `src/process/replay.rs`.
//!
//! For the **crash-restart re-dispatch** path via `SagaEffect::tell` (factory
//! registered, crash-restart bytes present), see
//! `saga_tell_crash_restart_redispatch.rs`.

#[path = "common/helpers.rs"]
mod common;
use common::{
    encode_outbox_tell_acked, encode_outbox_tell_requested,
    encode_outbox_tell_requested_with_target, outbox_kind_of, JsonCodec, OutboxKind, OUTBOX_MARKER,
};

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use futures_util::TryStreamExt;
use serde::{Deserialize, Serialize};

use nitinol_eventsource::SystemEvent;
use nitinol_eventsource::{system::EventSourceSystem, Event, SequenceCursor};
use nitinol_persistence::store::{EventStore, InMemoryEventStore};
use nitinol_persistence::{
    AppendingEvent, EventType, Family, LoadQuery, LoadedEvent, TypeName, Variant,
};
use nitinol_runtime::ProcessSystem;
use nitinol_saga::{
    DeadLetterEvent, Saga, SagaContext, SagaEffect, SagaFailure, SagaId, SagaProps,
};

#[derive(Clone, Debug, Serialize, Deserialize)]
struct OrderPlaced {
    sku: String,
}

impl Event for OrderPlaced {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("replay"), TypeName::new("OrderPlaced"));
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ReservationRequested {
    sku: String,
}

impl Event for ReservationRequested {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("replay"), TypeName::new("ReservationRequested"));
}

/// Correlation rule of [`InertSaga`]: the one instance each replay scenario
/// spawns.  No upstream event is ever delivered here — the subscriptions point
/// at an empty stream — but `correlate` has no default body, so the rule must
/// still be stated.
const INERT_SAGA_ID: &str = "replay-acked-saga-1";

/// Inert saga used only to drive the on_start replay path — never invoked.
#[derive(Default)]
struct InertSaga;

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

#[tokio::test]
async fn acked_tell_requested_does_not_get_synthetic_failed_on_replay() {
    // Negative-case companion: if both TellRequested AND TellAcked are present
    // before replay, no extra outbox event must be appended.

    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::new(ps).with_codec::<JsonCodec>().build();

    let saga_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let saga_id = SagaId::new(INERT_SAGA_ID);

    let pending_tell_id_payload = encode_outbox_tell_requested(2, None);
    let ack_payload = encode_outbox_tell_acked(2);

    append_raw(
        &saga_store,
        saga_id.as_str(),
        1,
        OUTBOX_MARKER,
        pending_tell_id_payload,
    )
    .await;
    append_raw(&saga_store, saga_id.as_str(), 2, OUTBOX_MARKER, ack_payload).await;

    let upstream_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());

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
            .spawn(system.process_system())
            .await;

    // Give the replay path time to (correctly) do nothing
    tokio::time::sleep(Duration::from_millis(300)).await;

    let events = load_saga_events(&saga_store, &saga_id).await;
    let failed_count = events
        .iter()
        .filter(|e| matches!(outbox_kind_of(e), Some(OutboxKind::TellFailed(_))))
        .count();
    let total_outbox = events
        .iter()
        .filter(|e| outbox_kind_of(e).is_some())
        .count();

    assert_eq!(
        failed_count, 0,
        "replay must not append TellFailed when the TellRequested already has an Ack"
    );
    assert_eq!(
        total_outbox, 2,
        "stream must still contain only the original TellRequested + TellAcked — \
         no extra outbox events must appear after a clean-state replay"
    );
}

/// When a `TellRequested` with **no** crash-restart bytes (written via
/// `TellIntent::new` directly, or from a stream predating crash-restart
/// support) is found on restart with no `PendingIntents` and no
/// crash-restart factory, the replay path must append a synthetic `TellFailed`
/// so the outbox stream reaches a consistent terminal state.
#[tokio::test]
async fn unresolvable_tell_requested_yields_synthetic_tell_failed_on_replay() {
    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::new(ps).with_codec::<JsonCodec>().build();

    let saga_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    // Each test owns its EventStore, so sharing the correlation id with the
    // negative-case test cannot make the two streams collide.
    let saga_id = SagaId::new(INERT_SAGA_ID);

    // Seed a TellRequested with tell_id = 1 and NO crash-restart bytes.
    // This simulates a `TellIntent::new` direct usage — prost TellRequested
    // with field 2 (crash_restart) absent.
    let tell_id_payload = encode_outbox_tell_requested(1, None);
    append_raw(
        &saga_store,
        saga_id.as_str(),
        1,
        OUTBOX_MARKER,
        tell_id_payload,
    )
    .await;

    let upstream_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());

    // Spawn without crash-restart factory and without pre-populated PendingIntents.
    // This simulates a full OS-process crash restart where no in-memory state survives.
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
            .spawn(system.process_system())
            .await;

    // Wait for the replay path to append the synthetic TellFailed.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let events = loop {
        let events = load_saga_events(&saga_store, &saga_id).await;
        let failed_count = events
            .iter()
            .filter(|e| matches!(outbox_kind_of(e), Some(OutboxKind::TellFailed(_))))
            .count();
        if failed_count >= 1 {
            break events;
        }
        if std::time::Instant::now() >= deadline {
            let event_types: Vec<_> = events.iter().map(|e| e.event_type.to_string()).collect();
            panic!(
                "timed out waiting for synthetic TellFailed after crash-restart replay \
                 (event_types: {:?})",
                event_types
            );
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    };

    let failed_count = events
        .iter()
        .filter(|e| matches!(outbox_kind_of(e), Some(OutboxKind::TellFailed(_))))
        .count();
    let acked_count = events
        .iter()
        .filter(|e| matches!(outbox_kind_of(e), Some(OutboxKind::TellAcked(_))))
        .count();

    assert_eq!(
        failed_count, 1,
        "replay must append exactly one synthetic TellFailed for each unresolvable \
         TellRequested (no crash-restart bytes, no PendingIntents, no factory)"
    );
    assert_eq!(
        acked_count, 0,
        "replay must NOT append TellAcked — the tell cannot be re-dispatched"
    );

    let failed_marker = events
        .iter()
        .find(|e| matches!(outbox_kind_of(e), Some(OutboxKind::TellFailed(_))))
        .expect("the synthetic TellFailed marker must be present");
    // Outbox markers carry a per-arm variant on the wire. The
    // synthetic TellFailed must write the `tell_failed` variant so the marker is
    // queryable by Materialized Path, while its variant-free `type_key` still
    // equals `OUTBOX_MARKER`'s so `classify`/routing keeps decoding it.
    assert_eq!(
        failed_marker.event_type.variant(),
        Some(Variant::new("tell_failed")),
        "the synthetic TellFailed must carry the per-arm `tell_failed` variant on the wire"
    );
    assert_eq!(
        failed_marker.event_type.type_key(),
        OUTBOX_MARKER.type_key(),
        "the marker's variant-free type_key must still match the reserved outbox key \
         so decode-registry routing keeps recovering the kind from the payload"
    );
}

/// Regression test: when a `TellRequested`
/// carries a non-empty `target` field (proto field 3, written after the DLQ
/// replay fix), the replay path must write a `DeadLetterEvent` with
/// `SagaFailure::TellFailed { target, .. }` in addition to the synthetic
/// `TellFailed` outbox marker.
#[tokio::test]
async fn unresolvable_tell_requested_with_target_enqueues_dead_letter_on_replay() {
    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::new(ps).with_codec::<JsonCodec>().build();

    let saga_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let saga_id = SagaId::new(INERT_SAGA_ID);

    // Seed a TellRequested (tell_id = 1) WITH target = "inventory-42".
    // This simulates a stream written after the `target` proto field was added —
    // the replay path must recover the target and write a DLQ entry.
    let payload = encode_outbox_tell_requested_with_target(1, None, "inventory-42");
    append_raw(&saga_store, saga_id.as_str(), 1, OUTBOX_MARKER, payload).await;

    let upstream_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    // Spawn without crash-restart factory and without pre-populated PendingIntents.
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
            .spawn(system.process_system())
            .await;

    // Wait for the replay path to append both the synthetic TellFailed and the
    // DLQ dead-letter event.
    let dead_letter_type_key = DeadLetterEvent::EVENT_TYPE.type_key();
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let events = loop {
        let events = load_saga_events(&saga_store, &saga_id).await;
        let has_failed = events
            .iter()
            .any(|e| matches!(outbox_kind_of(e), Some(OutboxKind::TellFailed(_))));
        let has_dead_letter = events
            .iter()
            .any(|e| e.event_type.type_key() == dead_letter_type_key);
        if has_failed && has_dead_letter {
            break events;
        }
        if std::time::Instant::now() >= deadline {
            let event_types: Vec<_> = events.iter().map(|e| e.event_type.to_string()).collect();
            panic!(
                "timed out waiting for synthetic TellFailed + DeadLetterEvent after replay \
                 (has_failed={has_failed}, has_dead_letter={has_dead_letter}, \
                 event_types: {:?})",
                event_types
            );
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    };

    let dead_letter_event = events
        .iter()
        .find(|e| e.event_type.type_key() == dead_letter_type_key)
        .expect("DeadLetterEvent must be present in the saga stream");

    let decoded = DeadLetterEvent::decode(&dead_letter_event.payload)
        .expect("DeadLetterEvent must decode successfully");

    match decoded.failure {
        SagaFailure::TellFailed { target, .. } => {
            assert_eq!(
                target,
                SagaId::new("inventory-42"),
                "DLQ TellFailed must carry the target recovered from the TellRequested proto field"
            );
        }
        other => panic!("DLQ entry must be TellFailed, got: {:?}", other),
    }
}

/// Regression test: when a `TellRequested` has **no**
/// `target` field (legacy stream predating proto field 3), the replay path
/// must write the synthetic `TellFailed` outbox marker but **must NOT** write
/// a `DeadLetterEvent`.  Emitting `TellFailed` with an empty target would
/// be invalid; the durable outbox marker already records the
/// failure without producing an invalid DLQ entry.
#[tokio::test]
async fn unresolvable_tell_requested_without_target_skips_dead_letter_on_replay() {
    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::new(ps).with_codec::<JsonCodec>().build();

    let saga_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let saga_id = SagaId::new(INERT_SAGA_ID);

    // Seed a TellRequested with NO target (legacy stream — field 3 absent / empty).
    let payload = encode_outbox_tell_requested(1, None);
    append_raw(&saga_store, saga_id.as_str(), 1, OUTBOX_MARKER, payload).await;

    let upstream_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    // Spawn without crash-restart factory and without pre-populated PendingIntents.
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
            .spawn(system.process_system())
            .await;

    // Wait for the synthetic TellFailed outbox marker.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let events = load_saga_events(&saga_store, &saga_id).await;
        let has_failed = events
            .iter()
            .any(|e| matches!(outbox_kind_of(e), Some(OutboxKind::TellFailed(_))));
        if has_failed {
            break;
        }
        if std::time::Instant::now() >= deadline {
            let event_types: Vec<_> = events.iter().map(|e| e.event_type.to_string()).collect();
            panic!(
                "timed out waiting for synthetic TellFailed from legacy stream (no target field) \
                 (event_types: {:?})",
                event_types
            );
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // Allow a brief settling period so any spurious DLQ write would have had time to appear.
    tokio::time::sleep(Duration::from_millis(200)).await;
    let events = load_saga_events(&saga_store, &saga_id).await;

    // No DLQ entry must be written for a legacy stream that lacks a target field —
    // emitting TellFailed with an empty target would be invalid.
    let dead_letter_type_key = DeadLetterEvent::EVENT_TYPE.type_key();
    let has_dead_letter = events
        .iter()
        .any(|e| e.event_type.type_key() == dead_letter_type_key);
    assert!(
        !has_dead_letter,
        "replay must NOT write a DLQ entry for a legacy TellRequested without a target field \
         (TellFailed.target must be non-empty)"
    );
}
