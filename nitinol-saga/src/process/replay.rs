//! Saga replay path with Outbox-aware classification.
//!
//! On `on_start` the saga loads its own event stream and walks it once:
//! - User events are decoded via the registered codec and `state.apply`-d.
//! - `TellRequested` markers add their `tell_id` plus any crash-restart bytes
//!   to the pending map.
//! - `TellAcked` / `TellFailed` markers remove the matching `tell_id` from
//!   the pending map.
//! - `Scheduled` markers are ignored in this MVP (scheduler execution is
//!   reserved for a follow-up issue).
//!
//! After the walk, any `tell_id` still in the pending map means a crash
//! between the atomic Persist batch and the executor's terminal Ack/Failed
//! append.
//!
//! ## Re-dispatch path — supervised restart (spec C-9)
//!
//! If the saga is restarted within the **same OS process** (e.g. under a
//! `Restart` supervision strategy), the `PendingIntents` registry still holds
//! the original `TellIntent`.  `replay_and_redispatch` pulls the intent out
//! of the registry and spawns a fresh retry executor — true re-dispatch per
//! spec C-9.
//!
//! ## Re-dispatch path — crash restart
//!
//! If the `PendingIntents` registry does not contain a matching entry (e.g.
//! after a full OS-process crash) but the `TellRequested` payload contains
//! crash-restart bytes **and** a crash-restart factory was registered via
//! [`crate::SagaProps::with_crash_restart_factory`], the factory is invoked
//! to reconstruct the `TellIntent` and spawn the retry executor.
//!
//! If no factory is registered or the payload carries no crash-restart bytes,
//! a **synthetic `TellFailed`** is appended to the saga stream so that the
//! outbox reaches a consistent terminal state.  The Saga can detect this
//! marker in a future `handle` call and trigger compensation (post-MVP).

use std::collections::HashMap;
use std::sync::Arc;

use bytes::Bytes;
use futures_util::StreamExt;
use nitinol_eventsource::codec::ErasedCodec;
use nitinol_persistence::store::EventStore;
use nitinol_persistence::{LoadQuery, LoadedEvent};
use tokio::sync::Mutex;

use crate::effect::TellIntent;
use crate::id::SagaId;
use crate::outbox::{
    decode_tell_id, decode_tell_requested, OutboxAppender, OutboxClassification, OutboxClassifier,
    TerminalKind,
};
use crate::outbox::RetryPolicy;
use crate::process::outbox_executor::spawn_outbox_executor;
use crate::process::pending_intents::PendingIntents;
use crate::saga::Saga;

/// Replay the saga's event stream, re-dispatch any pending tells, and return
/// the `tell_id`s of tells that were definitively failed in the event history.
///
/// The returned `Vec<u64>` lets the caller pre-populate the saga process's
/// shared `failed_tell_ids` accumulator so that the first `Saga::handle`
/// invocation after restart can inspect them via
/// [`crate::SagaContext::failed_tell_ids`].
///
/// `failed_tell_ids` is the saga process's shared accumulator.  Re-spawned
/// outbox executors write to it when they append a `TellFailed` terminal
/// marker, so that asynchronous TellFailed outcomes are also surfaced to
/// subsequent `Saga::handle` calls in the current run.
pub(crate) async fn replay_and_redispatch<S: Saga>(
    saga_id: &SagaId,
    state: &mut S,
    codec: &dyn ErasedCodec<S::Event>,
    store: &Arc<dyn EventStore>,
    sequence: &Arc<Mutex<u64>>,
    pending_intents: &PendingIntents,
    crash_restart_factory: Option<&(dyn Fn(&[u8]) -> Option<TellIntent> + Send + Sync)>,
    retry_policy: RetryPolicy,
    failed_tell_ids: Arc<Mutex<Vec<u64>>>,
) -> Vec<u64> {
    let scan = match scan_stream(saga_id, state, codec, store, sequence).await {
        Some(s) => s,
        None => return Vec::new(),
    };
    let failed = scan.failed.clone();
    redispatch_pending(
        saga_id,
        store,
        sequence,
        scan.pending,
        pending_intents,
        crash_restart_factory,
        retry_policy,
        failed_tell_ids,
    )
    .await;
    failed
}

/// Result of scanning the saga's event stream.
struct ReplayScan {
    /// Pending tell_ids (TellRequested without a terminal marker) mapped to
    /// their optional crash-restart payload bytes.
    pending: HashMap<u64, Option<Bytes>>,
    /// Tell_ids whose `TellFailed` terminal marker was found in the event
    /// stream.  Surfaced to the next `Saga::handle` call so the saga can
    /// detect unrecoverable tell failures and trigger compensation.
    failed: Vec<u64>,
}

async fn scan_stream<S: Saga>(
    saga_id: &SagaId,
    state: &mut S,
    codec: &dyn ErasedCodec<S::Event>,
    store: &Arc<dyn EventStore>,
    sequence: &Arc<Mutex<u64>>,
) -> Option<ReplayScan> {
    let initial_seq = *sequence.lock().await;
    let query = LoadQuery {
        stream_key: Some(saga_id.as_str().to_owned()),
        from_stream_sequence: Some(initial_seq + 1),
        ..Default::default()
    };
    let stream = match store.load(query).await {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(error = ?e, "saga event store load failed during replay");
            return None;
        }
    };

    futures_util::pin_mut!(stream);
    let mut highest_seq = initial_seq;
    // Maps pending tell_id → optional crash-restart payload bytes.
    let mut pending: HashMap<u64, Option<Bytes>> = HashMap::new();
    // Collect tell_ids that have a TellFailed terminal in the event stream.
    let mut failed: Vec<u64> = Vec::new();
    while let Some(item) = stream.next().await {
        let loaded = match item {
            Ok(ev) => ev,
            Err(e) => {
                tracing::error!(error = ?e, "saga event store stream error during replay");
                return None;
            }
        };
        highest_seq = highest_seq.max(loaded.sequence);
        dispatch_loaded::<S>(loaded, state, codec, &mut pending, &mut failed);
    }
    *sequence.lock().await = highest_seq;
    Some(ReplayScan { pending, failed })
}

fn dispatch_loaded<S: Saga>(
    loaded: LoadedEvent,
    state: &mut S,
    codec: &dyn ErasedCodec<S::Event>,
    pending: &mut HashMap<u64, Option<Bytes>>,
    failed: &mut Vec<u64>,
) {
    match OutboxClassifier::classify(loaded.event_type) {
        OutboxClassification::User => match codec.decode(&loaded.payload) {
            Ok(event) => state.apply(event),
            Err(e) => {
                tracing::error!(error = %e, "saga event decode failed; skipping event");
            }
        },
        OutboxClassification::TellRequested => {
            // Decode tell_id and optional crash-restart payload.
            if let Some((id, crp)) = decode_tell_requested(&loaded.payload) {
                pending.insert(id, crp);
            }
        }
        OutboxClassification::TellAcked => {
            if let Some(id) = decode_tell_id(&loaded.payload) {
                pending.remove(&id);
            }
        }
        OutboxClassification::TellFailed => {
            if let Some(id) = decode_tell_id(&loaded.payload) {
                pending.remove(&id);
                // Record the failure so it can be surfaced to the next
                // Saga::handle call (spec C-9: saga detects TellFailed
                // after restart and can trigger compensation).
                failed.push(id);
            }
        }
        OutboxClassification::Scheduled => { /* no-op MVP */ }
    }
}

/// For each pending `(tell_id, crash_restart_payload)`, resolve the `TellIntent`
/// via one of two paths:
///
/// 1. **Supervised restart** — `PendingIntents` still holds the original
///    intent (same OS-process lifetime).  Pull it out and spawn a fresh retry
///    executor.
/// 2. **Crash restart** — `PendingIntents` is empty (new OS process).  If the
///    `TellRequested` payload carried crash-restart bytes **and** a factory was
///    registered via [`crate::SagaProps::with_crash_restart_factory`], invoke
///    the factory to reconstruct the intent and spawn the retry executor.
///
/// If neither path succeeds, a **synthetic `TellFailed`** is appended so that
/// the outbox reaches a consistent terminal state.  This covers cases where
/// `TellIntent::new` was used directly (no crash-restart bytes), or where
/// `SagaEffect::tell` was used but no crash-restart factory was registered.
/// The Saga can detect the resulting `TellFailed` event in a subsequent
/// `handle` call and compensate.
async fn redispatch_pending(
    saga_id: &SagaId,
    store: &Arc<dyn EventStore>,
    sequence: &Arc<Mutex<u64>>,
    pending: HashMap<u64, Option<Bytes>>,
    pending_intents: &PendingIntents,
    crash_restart_factory: Option<&(dyn Fn(&[u8]) -> Option<TellIntent> + Send + Sync)>,
    retry_policy: RetryPolicy,
    failed_tell_ids: Arc<Mutex<Vec<u64>>>,
) {
    for (tell_id, crash_restart_payload) in pending {
        // Attempt to resolve the TellIntent via one of two paths.
        //
        // IMPORTANT: do NOT call `pending_intents.take` here.  The intent
        // must stay in the real registry until the re-spawned executor
        // successfully appends the terminal marker.  The executor calls
        // `pending_intents.take` only after a durable append; keeping the
        // entry intact means a subsequent supervised restart can still
        // re-dispatch if the terminal append fails.
        let resolved = if let Some(intent) = pending_intents.get(tell_id).await {
            // Supervised restart: in-memory intent is still available.
            // Use `get` (clone without removing) so the real registry entry
            // survives until the executor's terminal append succeeds.
            tracing::debug!(
                tell_id,
                "saga replay: supervised restart — re-dispatching pending tell"
            );
            Some(intent)
        } else if let (Some(factory), Some(payload)) =
            (crash_restart_factory, crash_restart_payload.as_deref())
        {
            // Crash restart: reconstruct via factory using the persisted bytes.
            match factory(payload) {
                Some(reconstructed) => {
                    tracing::debug!(
                        tell_id,
                        "saga replay: crash restart — reconstructed TellIntent from payload"
                    );
                    // Register the reconstructed intent in the real registry
                    // before spawning so the executor can remove it after a
                    // successful terminal append.
                    pending_intents.register(tell_id, reconstructed.clone()).await;
                    Some(reconstructed)
                }
                None => {
                    tracing::warn!(
                        tell_id,
                        "saga replay: crash restart factory returned None for tell_id; \
                         appending synthetic TellFailed"
                    );
                    None
                }
            }
        } else {
            // Neither supervised-restart intent nor crash-restart path is available.
            // This path is taken for TellRequested markers written via
            // `TellIntent::new` directly (no crash-restart bytes), or when
            // `SagaEffect::tell` was used but no factory was registered.
            tracing::warn!(
                tell_id,
                has_factory = crash_restart_factory.is_some(),
                has_payload = crash_restart_payload.is_some(),
                "saga replay: pending TellRequested cannot be re-dispatched \
                 (configure crash-restart factory and crash-restart payload to \
                 enable crash-restart re-dispatch); appending synthetic TellFailed"
            );
            None
        };

        if let Some(intent) = resolved {
            spawn_outbox_executor(
                intent,
                tell_id,
                retry_policy.clone(),
                saga_id.clone(),
                Arc::clone(store),
                Arc::clone(sequence),
                // Pass the real registry so the executor removes the entry
                // only after the terminal marker is durably appended.
                pending_intents.clone(),
                // Replay-spawned executors share the same failed-tell-ids
                // accumulator as runtime executors so that TellFailed outcomes
                // surface to subsequent Saga::handle calls regardless of origin.
                Arc::clone(&failed_tell_ids),
            );
        } else {
            // No path resolved the intent.  Write a synthetic TellFailed so the
            // outbox stream is in a consistent terminal state rather than
            // leaving an orphaned TellRequested indefinitely.
            let appended = OutboxAppender::append_terminal(
                store,
                saga_id,
                sequence,
                TerminalKind::Failed,
                tell_id,
            )
            .await;
            if appended {
                // Surface the synthetic TellFailed to the next Saga::handle
                // call via the shared accumulator, mirroring what the runtime
                // executor path does when it appends TellFailed after exhausting
                // retries.
                failed_tell_ids.lock().await.push(tell_id);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    //! Spec C-9 / replay path — **supervised-restart re-dispatch**: when the
    //! saga is restarted within the same OS process (e.g. under a `Restart`
    //! supervision strategy) and the in-memory [`PendingIntents`] registry still
    //! holds the original [`TellIntent`], the replay path must re-dispatch the
    //! intent to a fresh retry executor instead of leaving it unresolved.
    //!
    //! These tests live here (inside the crate) rather than in `tests/` so they
    //! can access `PendingIntents` and `SagaProps::with_pending_intents`, both
    //! of which are `pub(crate)` implementation details.

    use std::pin::Pin;
    use std::sync::Arc;
    use std::time::Duration;

    use async_trait::async_trait;
    use bytes::Bytes;
    use futures_core::Stream;
    use futures_util::TryStreamExt;
    use serde::{Deserialize, Serialize};

    use nitinol_eventsource::codec::Codec;
    use nitinol_eventsource::test_helpers::MockAggregateProxy;
    use nitinol_eventsource::{
        system::EventSourceSystem, Aggregate, Context, Decider, Effect, Event, SequenceCursor,
    };
    use nitinol_persistence::error::{AppendError, LoadError};
    use nitinol_persistence::store::{EventStore, EventStream, InMemoryEventStore};
    use nitinol_persistence::{AppendOutcome, AppendingEvent, EventType, LoadQuery, LoadedEvent};
    use nitinol_runtime::ProcessSystem;

    use crate::process::pending_intents::PendingIntents;
    use crate::{Saga, SagaContext, SagaEffect, SagaId, SagaProps, TellIntent};

    // -----------------------------------------------------------------------
    // Store that succeeds for load but fails every append call.
    // Used to test that the replay path does not remove the pending intent
    // from the registry before the terminal append succeeds (ARCH-45-001).
    // -----------------------------------------------------------------------

    struct FailAllAppendStore {
        events: Vec<LoadedEvent>,
    }

    impl FailAllAppendStore {
        /// Build a store pre-seeded with a single `TellRequested` event for
        /// `tell_id` in the given saga stream.
        fn with_tell_requested(saga_stream_key: &str, tell_id: u64) -> Self {
            let payload = Bytes::from(tell_id.to_be_bytes().to_vec());
            Self {
                events: vec![LoadedEvent {
                    stream_key: saga_stream_key.to_owned(),
                    sequence: 1,
                    global_sequence: 1,
                    event_type: EventType::from_str("nitinol.saga.outbox.tell_requested"),
                    payload,
                    occurred_at: jiff::Timestamp::UNIX_EPOCH,
                }],
            }
        }
    }

    #[async_trait]
    impl EventStore for FailAllAppendStore {
        async fn append(
            &self,
            _key: &str,
            _events: Vec<AppendingEvent>,
        ) -> Result<AppendOutcome, AppendError> {
            Err(AppendError::Backend(
                "injected: all appends fail (ARCH-45-001 test)".into(),
            ))
        }

        async fn load(&self, query: LoadQuery) -> Result<EventStream<'_>, LoadError> {
            let items: Vec<Result<LoadedEvent, LoadError>> = self
                .events
                .iter()
                .filter(|e| {
                    if let Some(ref key) = query.stream_key {
                        if &e.stream_key != key {
                            return false;
                        }
                    }
                    if let Some(from_seq) = query.from_stream_sequence {
                        if e.sequence < from_seq {
                            return false;
                        }
                    }
                    true
                })
                .cloned()
                .map(Ok)
                .collect();
            let s: Pin<Box<dyn Stream<Item = Result<LoadedEvent, LoadError>> + Send + '_>> =
                Box::pin(futures_util::stream::iter(items));
            Ok(s)
        }
    }

    const OUTBOX_TELL_ACKED: &str = "nitinol.saga.outbox.tell_acked";
    const OUTBOX_TELL_FAILED: &str = "nitinol.saga.outbox.tell_failed";

    // -----------------------------------------------------------------------
    // Minimal JSON codec for tests
    // -----------------------------------------------------------------------

    #[derive(Default)]
    struct JsonCodec;

    impl<E: Serialize + for<'de> Deserialize<'de>> Codec<E> for JsonCodec {
        type Error = serde_json::Error;

        fn encode(event: &E) -> Result<Bytes, Self::Error> {
            serde_json::to_vec(event).map(Bytes::from)
        }

        fn decode(payload: &[u8]) -> Result<E, Self::Error> {
            serde_json::from_slice(payload)
        }
    }

    // -----------------------------------------------------------------------
    // Domain types
    // -----------------------------------------------------------------------

    #[derive(Clone, Debug, Serialize, Deserialize)]
    struct OrderPlaced {
        sku: String,
    }

    impl Event for OrderPlaced {
        const EVENT_TYPE: EventType = EventType::from_str("redispatch.OrderPlaced");
    }

    #[derive(Clone, Debug, Serialize, Deserialize)]
    struct ReservationRequested {
        sku: String,
    }

    impl Event for ReservationRequested {
        const EVENT_TYPE: EventType = EventType::from_str("redispatch.ReservationRequested");
    }

    #[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
    struct Reserved {
        sku: String,
    }

    impl Event for Reserved {
        const EVENT_TYPE: EventType = EventType::from_str("redispatch.Reserved");
    }

    #[derive(Default)]
    struct Inventory;

    impl Aggregate for Inventory {
        type Event = Reserved;

        fn apply(&mut self, _event: Reserved) {}
    }

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
                sku: "redispatch-sku".to_owned(),
            }))
        }
    }

    // -----------------------------------------------------------------------
    // Inert saga — only the on_start replay path is exercised
    // -----------------------------------------------------------------------

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

    // -----------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------

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

    async fn load_saga_events(
        store: &Arc<dyn EventStore>,
        saga_id: &SagaId,
    ) -> Vec<LoadedEvent> {
        store
            .load(LoadQuery::by_stream(saga_id))
            .await
            .expect("load saga stream must succeed")
            .try_collect()
            .await
            .expect("collect saga events must succeed")
    }

    // -----------------------------------------------------------------------
    // Test
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn unacked_tell_requested_with_pending_intent_yields_tell_acked_on_replay() {
        // Build a mock aggregate proxy for Inventory.
        let mock = MockAggregateProxy::<Inventory>::new();

        // Create the shared PendingIntents registry.  This simulates the
        // in-memory state that would survive a supervised restart.
        let pending_intents = PendingIntents::new();

        // Pre-register the TellIntent for tell_id = 1 so the replay path can
        // find it and re-dispatch instead of leaving it unresolved.
        let intent = TellIntent::new::<Inventory, Reserve, _>(mock.clone(), Reserve);
        pending_intents.register(1, intent).await;

        let ps = ProcessSystem::new().await;
        let system = EventSourceSystem::new(ps)
            .with_codec::<JsonCodec>()
            .build();

        let saga_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
        let saga_id = SagaId::new("redispatch-replay-saga-1");

        // Seed a TellRequested with tell_id = 1 (8-byte big-endian payload).
        let tell_id_payload = Bytes::from(1u64.to_be_bytes().to_vec());
        append_raw(
            &saga_store,
            saga_id.as_str(),
            1,
            EventType::from_str("nitinol.saga.outbox.tell_requested"),
            tell_id_payload,
        )
        .await;

        let upstream_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());

        let routed = saga_id.clone();
        let route_fn =
            move |_event: &OrderPlaced| -> Option<SagaId> { Some(routed.clone()) };

        let _saga_proxy = SagaProps::<InertSaga>::new(
            saga_id.clone(),
            Arc::clone(&saga_store),
            InertSaga::default,
        )
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
        .with_pending_intents(pending_intents)
        .spawn(system.process_system())
        .await;

        // Wait for the replay re-dispatch to produce TellAcked.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let events = loop {
            let events = load_saga_events(&saga_store, &saga_id).await;
            let acked_count = events
                .iter()
                .filter(|e| e.event_type.as_str() == OUTBOX_TELL_ACKED)
                .count();
            if acked_count >= 1 {
                break events;
            }
            if std::time::Instant::now() >= deadline {
                let event_types: Vec<_> =
                    events.iter().map(|e| e.event_type.as_str()).collect();
                panic!(
                    "timed out waiting for TellAcked on replay re-dispatch \
                     (event_types: {:?})",
                    event_types
                );
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        };

        let acked_count = events
            .iter()
            .filter(|e| e.event_type.as_str() == OUTBOX_TELL_ACKED)
            .count();
        let failed_count = events
            .iter()
            .filter(|e| e.event_type.as_str() == OUTBOX_TELL_FAILED)
            .count();

        assert_eq!(
            acked_count, 1,
            "replay re-dispatch must produce exactly one TellAcked \
             for the one pending TellIntent"
        );
        assert_eq!(
            failed_count, 0,
            "replay re-dispatch must NOT produce TellFailed — the in-memory \
             intent was available so the retry executor should have succeeded"
        );
    }

    /// Regression test for ARCH-45-001 (call-chain-integrity).
    ///
    /// The replay path must NOT remove the pending intent from the real
    /// `PendingIntents` registry before the re-spawned executor's terminal
    /// append succeeds.  Previously `pending_intents.take(tell_id)` was called
    /// before spawning, erasing the real registry entry unconditionally.  If
    /// the subsequent `append_terminal` failed, the next supervised restart
    /// could no longer re-dispatch because the intent was already gone.
    ///
    /// With the fix (`get` instead of `take` + real registry passed to executor),
    /// the intent stays in the registry until the executor's terminal append
    /// completes successfully.
    #[tokio::test]
    async fn replay_keeps_pending_intent_when_terminal_append_fails() {
        let mock = MockAggregateProxy::<Inventory>::new();
        let pending_intents = PendingIntents::new();

        // Pre-register the intent — simulates a supervised-restart where the
        // real in-memory registry still has the entry.
        let intent = TellIntent::new::<Inventory, Reserve, _>(mock.clone(), Reserve);
        pending_intents.register(1, intent).await;

        let ps = ProcessSystem::new().await;
        let system = EventSourceSystem::new(ps)
            .with_codec::<JsonCodec>()
            .build();

        let saga_id = SagaId::new("arch-45-001-saga-1");
        // FailAllAppendStore has TellRequested(tell_id=1) pre-loaded and fails
        // every append — so the executor's terminal append will fail.
        let saga_store: Arc<dyn EventStore> =
            Arc::new(FailAllAppendStore::with_tell_requested(saga_id.as_str(), 1));

        let upstream_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
        let routed = saga_id.clone();
        let route_fn =
            move |_event: &OrderPlaced| -> Option<SagaId> { Some(routed.clone()) };

        let _saga_proxy = SagaProps::<InertSaga>::new(
            saga_id.clone(),
            saga_store,
            InertSaga::default,
        )
        .with_codec(system.codec::<ReservationRequested>())
        .with_subscription(
            upstream_store,
            system.codec::<OrderPlaced>(),
            SequenceCursor::Stream {
                key: "no-such-stream".to_owned(),
                after: 0,
            },
            route_fn,
        )
        .with_pending_intents(pending_intents.clone())
        .spawn(system.process_system())
        .await;

        // Give the executor time to exhaust its retries and attempt (but fail)
        // the terminal append.  RetryPolicy::default() has a small max_attempts
        // with short backoff, so 1 second is ample.
        tokio::time::sleep(Duration::from_secs(1)).await;

        let entry_present = pending_intents.0.lock().await.contains_key(&1);
        assert!(
            entry_present,
            "ARCH-45-001 regression: the pending intent must remain in the real \
             PendingIntents registry when the terminal append fails; a subsequent \
             supervised restart must still be able to re-dispatch the tell"
        );
    }
}
