//! Spec C-9 / replay path — synthetic `TellFailed` and negative cases.
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

mod common;
use common::{encode_tell_id, encode_tell_requested, JsonCodec};

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use futures_util::TryStreamExt;
use serde::{Deserialize, Serialize};

use nitinol_eventsource::{system::EventSourceSystem, Event, SequenceCursor};
use nitinol_persistence::store::{EventStore, InMemoryEventStore};
use nitinol_persistence::{AppendingEvent, EventType, Family, LoadQuery, LoadedEvent, TypeName};
use nitinol_runtime::ProcessSystem;
use nitinol_saga::{Saga, SagaContext, SagaEffect, SagaId, SagaProps};

const OUTBOX_PREFIX: &str = "nitinol.saga.outbox.";
const OUTBOX_TELL_REQUESTED: EventType = EventType::new(
    Family::new("nitinol.saga.outbox"),
    TypeName::new("tell_requested"),
);
const OUTBOX_TELL_ACKED: EventType =
    EventType::new(Family::new("nitinol.saga.outbox"), TypeName::new("tell_acked"));
const OUTBOX_TELL_FAILED: EventType =
    EventType::new(Family::new("nitinol.saga.outbox"), TypeName::new("tell_failed"));

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
    const EVENT_TYPE: EventType = EventType::new(
        Family::new("replay"),
        TypeName::new("ReservationRequested"),
    );
}

/// Inert saga used only to drive the on_start replay path — never invoked.
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
    let saga_id = SagaId::new("replay-acked-saga-1");

    let pending_tell_id_payload = encode_tell_requested(2, None);
    let ack_payload = encode_tell_id(2);

    append_raw(
        &saga_store,
        saga_id.as_str(),
        1,
        OUTBOX_TELL_REQUESTED,
        pending_tell_id_payload,
    )
    .await;
    append_raw(
        &saga_store,
        saga_id.as_str(),
        2,
        OUTBOX_TELL_ACKED,
        ack_payload,
    )
    .await;

    let upstream_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());

    let routed = saga_id.clone();
    let route_fn = move |_event: &OrderPlaced| -> Option<SagaId> { Some(routed.clone()) };

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
            .spawn(system.process_system())
            .await;

    // Give the replay path time to (correctly) do nothing
    tokio::time::sleep(Duration::from_millis(300)).await;

    let events = load_saga_events(&saga_store, &saga_id).await;
    let failed_count = events
        .iter()
        .filter(|e| e.event_type == OUTBOX_TELL_FAILED)
        .count();
    let total_outbox = events
        .iter()
        .filter(|e| e.event_type.to_string().starts_with(OUTBOX_PREFIX))
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
    // Use a unique saga id to avoid collisions with the negative-case test.
    let saga_id = SagaId::new("replay-unresolvable-saga-1");

    // Seed a TellRequested with tell_id = 1 and NO crash-restart bytes.
    // This simulates a `TellIntent::new` direct usage — prost TellRequested
    // with field 2 (crash_restart) absent.
    let tell_id_payload = encode_tell_requested(1, None);
    append_raw(
        &saga_store,
        saga_id.as_str(),
        1,
        OUTBOX_TELL_REQUESTED,
        tell_id_payload,
    )
    .await;

    let upstream_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());

    let routed = saga_id.clone();
    let route_fn = move |_event: &OrderPlaced| -> Option<SagaId> { Some(routed.clone()) };

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
                route_fn,
            )
            .spawn(system.process_system())
            .await;

    // Wait for the replay path to append the synthetic TellFailed.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let events = loop {
        let events = load_saga_events(&saga_store, &saga_id).await;
        let failed_count = events
            .iter()
            .filter(|e| e.event_type == OUTBOX_TELL_FAILED)
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
        .filter(|e| e.event_type == OUTBOX_TELL_FAILED)
        .count();
    let acked_count = events
        .iter()
        .filter(|e| e.event_type == OUTBOX_TELL_ACKED)
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
}
