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
//! `Restart` supervision strategy), the in-memory `pending_intents` map still
//! holds the original `TellIntent`.  `replay_and_redispatch` reads the intent
//! out of the map and spawns a fresh retry executor — true re-dispatch per
//! spec C-9.
//!
//! ## Re-dispatch path — crash restart
//!
//! If the in-memory `pending_intents` map does not contain a matching entry
//! (e.g. after a full OS-process crash) but the `TellRequested` payload
//! contains crash-restart bytes **and** a crash-restart factory was registered
//! via [`crate::SagaProps::with_crash_restart_factory`], the factory is
//! invoked to reconstruct the `TellIntent` and spawn the retry executor.
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
use nitinol_runtime::process::ProcessProxy;

use crate::effect::TellIntent;
use crate::id::SagaId;
use crate::outbox::RetryPolicy;
use crate::outbox::{
    decode_tell_id, decode_tell_requested, OutboxAppender, OutboxClassification, OutboxClassifier,
    TerminalKind,
};
use crate::process::outbox_executor::spawn_outbox_executor;
use crate::process::saga_process::SagaProcess;
use crate::saga::Saga;

/// Replay the saga's event stream, re-dispatch any pending tells, and return
/// the `tell_id`s of tells that were definitively failed in the event history.
///
/// The returned `Vec<u64>` lets the caller pre-populate the saga process's
/// `failed_tell_ids` accumulator so that the first `Saga::handle` invocation
/// after restart can inspect them via
/// [`crate::SagaContext::failed_tell_ids`].
///
/// Re-spawned outbox executors notify the saga via `OutboxTerminalSettled`
/// when they append a `TellFailed` terminal marker, so that asynchronous
/// TellFailed outcomes are also surfaced to subsequent `Saga::handle` calls in
/// the current run.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn replay_and_redispatch<S: Saga>(
    saga_id: &SagaId,
    state: &mut S,
    codec: &dyn ErasedCodec<S::Event>,
    store: &Arc<dyn EventStore>,
    sequence: &mut u64,
    pending_intents: &mut HashMap<u64, TellIntent>,
    crash_restart_factory: Option<&(dyn Fn(&[u8]) -> Option<TellIntent> + Send + Sync)>,
    retry_policy: RetryPolicy,
    self_proxy: ProcessProxy<SagaProcess<S>>,
) -> Vec<u64> {
    let scan = match scan_stream(saga_id, state, codec, store, sequence).await {
        Some(s) => s,
        None => return Vec::new(),
    };
    let mut failed = scan.failed;
    let synthetic_failed = redispatch_pending(
        saga_id,
        store,
        sequence,
        scan.pending,
        pending_intents,
        crash_restart_factory,
        retry_policy,
        self_proxy,
    )
    .await;
    // Surface synthetic-TellFailed tell_ids the same way as TellFailed markers
    // already in the stream: the next `Saga::handle` must observe both via
    // `SagaContext::failed_tell_ids` (ARCH-45-002 regression).
    failed.extend(synthetic_failed);
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
    sequence: &mut u64,
) -> Option<ReplayScan> {
    let initial_seq = *sequence;
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
    *sequence = highest_seq;
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
/// 1. **Supervised restart** — `pending_intents` still holds the original
///    intent (same OS-process lifetime).  Read it and spawn a fresh retry
///    executor.
/// 2. **Crash restart** — `pending_intents` is empty (new OS process).  If the
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
#[allow(clippy::too_many_arguments)]
async fn redispatch_pending<S: Saga>(
    saga_id: &SagaId,
    store: &Arc<dyn EventStore>,
    sequence: &mut u64,
    pending: HashMap<u64, Option<Bytes>>,
    pending_intents: &mut HashMap<u64, TellIntent>,
    crash_restart_factory: Option<&(dyn Fn(&[u8]) -> Option<TellIntent> + Send + Sync)>,
    retry_policy: RetryPolicy,
    self_proxy: ProcessProxy<SagaProcess<S>>,
) -> Vec<u64> {
    let mut synthetic_failed: Vec<u64> = Vec::new();
    for (tell_id, crash_restart_payload) in pending {
        // Attempt to resolve the TellIntent via one of two paths.
        //
        // IMPORTANT: do NOT remove the entry from `pending_intents` here.  The
        // intent must stay in the registry until the re-spawned executor
        // successfully appends the terminal marker.  The executor notifies the
        // saga via `OutboxTerminalSettled` only after a durable append; keeping
        // the entry intact means a subsequent supervised restart can still
        // re-dispatch if the terminal append fails.
        let resolved = if let Some(intent) = pending_intents.get(&tell_id).cloned() {
            // Supervised restart: in-memory intent is still available.
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
                    // Register the reconstructed intent so the executor's
                    // terminal-settled notification can clean it up.
                    pending_intents.insert(tell_id, reconstructed.clone());
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
            spawn_outbox_executor(intent, tell_id, retry_policy.clone(), self_proxy.clone());
        } else {
            // No path resolved the intent.  Claim a fresh sequence here from
            // the local cursor (we are still inside `on_start`, so no other
            // task is touching it) and append a synthetic TellFailed so the
            // outbox stream is in a consistent terminal state.
            *sequence += 1;
            let claimed = *sequence;
            let appended = OutboxAppender::append_terminal(
                store,
                saga_id,
                claimed,
                TerminalKind::Failed,
                tell_id,
            )
            .await;
            if appended {
                // Surface the synthetic-TellFailed tell_id so the first
                // `Saga::handle` invocation after `on_start` observes it via
                // `SagaContext::failed_tell_ids` — matching the runtime
                // executor path's behaviour when it appends TellFailed after
                // exhausting retries.
                synthetic_failed.push(tell_id);
            } else {
                // Roll back the cursor so the next attempt does not skip seq.
                *sequence -= 1;
            }
        }
    }

    synthetic_failed
}

// ---------------------------------------------------------------------------
// Tests — supervised-restart re-dispatch regression
//
// Referenced from `nitinol-saga/tests/saga_outbox_replay_synthesizes_failed.rs`
// where the companion synthetic-`TellFailed` integration tests live.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::pin::Pin;
    use std::sync::Arc;
    use std::time::Duration;

    use async_trait::async_trait;
    use bytes::Bytes;
    use futures_core::Stream;
    use futures_util::TryStreamExt;
    use serde::{Deserialize, Serialize};

    use nitinol_eventsource::codec::{Codec, ErasedCodec};
    use nitinol_eventsource::test_helpers::MockAggregateProxy;
    use nitinol_eventsource::{Aggregate, Context, Decider, Effect, Event, SequenceCursor};
    use nitinol_persistence::error::{AppendError, LoadError};
    use nitinol_persistence::store::{EventStore, EventStream, InMemoryEventStore};
    use nitinol_persistence::{AppendOutcome, LoadQuery};
    use nitinol_persistence::{AppendingEvent, EventType, LoadedEvent};
    use nitinol_runtime::ProcessSystem;

    use crate::outbox::RetryPolicy;
    use crate::process::saga_process::SagaProcess;
    use crate::{Saga, SagaContext, SagaEffect, SagaId, SagaProps, TellIntent};

    // -----------------------------------------------------------------------
    // Minimal JSON codec for use inside the test module (mirrors common/ in
    // integration tests but is defined inline so library unit tests don't
    // depend on the integration-test helper path).
    // -----------------------------------------------------------------------

    struct JsonCodec;

    impl<E: Serialize + for<'de> Deserialize<'de> + 'static> Codec<E> for JsonCodec {
        type Error = serde_json::Error;

        fn encode(event: &E) -> Result<Bytes, Self::Error> {
            serde_json::to_vec(event).map(Bytes::from)
        }

        fn decode(payload: &[u8]) -> Result<E, Self::Error> {
            serde_json::from_slice(payload)
        }
    }

    // -----------------------------------------------------------------------
    // A minimal aggregate whose sole purpose is to accept a command via
    // `TellIntent::new` so the executor has a real proxy to call.
    // -----------------------------------------------------------------------

    #[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
    struct MarkerEvent;

    impl Event for MarkerEvent {
        const EVENT_TYPE: EventType = EventType::from_str("replay_unit_test.Marker");
    }

    #[derive(Default)]
    struct MarkerAggregate;

    impl Aggregate for MarkerAggregate {
        type Event = MarkerEvent;

        fn apply(&mut self, _event: MarkerEvent) {}
    }

    #[derive(Clone)]
    struct MarkerCmd;

    #[async_trait]
    impl Decider<MarkerCmd> for MarkerAggregate {
        type Rejection = std::convert::Infallible;

        async fn decide(
            &self,
            _cmd: MarkerCmd,
            _ctx: &mut Context,
        ) -> Result<Effect<MarkerEvent>, Self::Rejection> {
            Ok(Effect::empty())
        }
    }

    // -----------------------------------------------------------------------
    // Inert saga — never handles upstream events; only the `on_start` replay
    // path is exercised in these tests.
    // -----------------------------------------------------------------------

    #[derive(Clone, Debug, Serialize, Deserialize)]
    struct UpstreamEvt;

    impl Event for UpstreamEvt {
        const EVENT_TYPE: EventType = EventType::from_str("replay_unit_test.Upstream");
    }

    #[derive(Clone, Debug, Serialize, Deserialize)]
    struct SagaEvt;

    impl Event for SagaEvt {
        const EVENT_TYPE: EventType = EventType::from_str("replay_unit_test.SagaEvent");
    }

    #[derive(Default)]
    struct InertSaga;

    #[async_trait]
    impl Saga for InertSaga {
        type SubscribedEvent = UpstreamEvt;
        type Event = SagaEvt;
        type State = ();
        type Error = std::convert::Infallible;

        fn apply(&mut self, _event: SagaEvt) {}

        async fn handle(
            &mut self,
            _event: UpstreamEvt,
            _ctx: &mut SagaContext,
        ) -> Result<SagaEffect<SagaEvt>, Self::Error> {
            Ok(SagaEffect::None)
        }
    }

    // -----------------------------------------------------------------------
    // EventStore that fails all `append` calls, used to exercise the path
    // where `AppendTerminalAndClaim` returns `false` and the executor exits
    // without sending `OutboxTerminalSettled`.
    // -----------------------------------------------------------------------

    struct FailAppendStore {
        inner: Arc<InMemoryEventStore>,
    }

    impl FailAppendStore {
        /// Build a store backed by `inner` whose `append` always fails.
        /// Pre-seed the inner store BEFORE wrapping to inject initial events.
        fn wrap(inner: Arc<InMemoryEventStore>) -> Arc<dyn EventStore> {
            Arc::new(Self { inner })
        }
    }

    #[async_trait]
    impl EventStore for FailAppendStore {
        async fn append(
            &self,
            _key: &str,
            _events: Vec<AppendingEvent>,
        ) -> Result<AppendOutcome, AppendError> {
            Err(AppendError::Backend(
                "injected failure: all appends fail".into(),
            ))
        }

        async fn load(&self, query: LoadQuery) -> Result<EventStream<'_>, LoadError> {
            // Collect owned events so the returned stream has 'static lifetime,
            // satisfying EventStream<'_> (a 'static stream is coercible to 'a).
            let events: Vec<LoadedEvent> = self.inner.load(query).await?.try_collect().await?;
            let stream: Pin<
                Box<dyn Stream<Item = Result<LoadedEvent, LoadError>> + Send + 'static>,
            > = Box::pin(futures_util::stream::iter(events.into_iter().map(Ok)));
            Ok(stream)
        }
    }

    // -----------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------

    fn encode_tell_requested_no_crash_bytes(tell_id: u64) -> Bytes {
        // Plain 8-byte big-endian tell_id with no appended crash-restart bytes.
        // `decode_tell_requested` treats the suffix (absent here) as None, so the
        // crash-restart path is unavailable for this intent — only the in-memory
        // supervised-restart path can re-dispatch it.
        Bytes::from(tell_id.to_be_bytes().to_vec())
    }

    async fn seed_tell_requested(
        store: &Arc<InMemoryEventStore>,
        saga_id: &SagaId,
        sequence: u64,
        tell_id: u64,
    ) {
        store
            .append(
                saga_id.as_str(),
                vec![AppendingEvent {
                    sequence,
                    event_type: EventType::from_str("nitinol.saga.outbox.tell_requested"),
                    payload: encode_tell_requested_no_crash_bytes(tell_id),
                    occurred_at: jiff::Timestamp::now(),
                }],
            )
            .await
            .expect("seed TellRequested must succeed");
    }

    async fn load_outbox_events(store: &Arc<dyn EventStore>, saga_id: &SagaId) -> Vec<LoadedEvent> {
        store
            .load(LoadQuery::by_stream(saga_id))
            .await
            .expect("load must succeed")
            .try_collect()
            .await
            .expect("collect must succeed")
    }

    // -----------------------------------------------------------------------
    // Test 1 — supervised-restart re-dispatch produces TellAcked
    //
    // Contract: when `pending_intents` carries the matching `TellIntent` for
    // an unacked `TellRequested` (simulating a supervised same-OS-process
    // restart), `replay_and_redispatch` must re-dispatch the intent and
    // eventually produce a durable `TellAcked` — NOT a synthetic `TellFailed`.
    // -----------------------------------------------------------------------

    /// Supervised restart path: when the in-memory `TellIntent` is available
    /// for an unacked `TellRequested` on replay, the saga re-dispatches the
    /// intent via a fresh outbox executor and the store receives `TellAcked`.
    #[tokio::test]
    async fn unacked_tell_requested_with_pending_intent_yields_tell_acked_on_replay() {
        let mock = MockAggregateProxy::<MarkerAggregate>::new();
        let ps = ProcessSystem::new().await;

        let inner_store = Arc::new(InMemoryEventStore::default());
        let saga_id = SagaId::new("supervised-replay-acked-unit-1");

        // Seed a TellRequested with tell_id=1 and NO crash-restart bytes.
        // Without crash bytes there is no crash-restart path; only the
        // in-memory supervised-restart path can re-dispatch it.
        seed_tell_requested(&inner_store, &saga_id, 1, 1).await;

        // Build an in-memory intent for tell_id=1 — simulates what the
        // previous `SagaProcess` run would have stored before crashing.
        let intent = TellIntent::new::<MarkerAggregate, MarkerCmd, _>(mock.clone(), MarkerCmd);
        let mut initial_intents = HashMap::new();
        initial_intents.insert(1u64, intent);

        let upstream_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
        let routed = saga_id.clone();
        let route_fn = move |_: &UpstreamEvt| Some(routed.clone());

        // Spawn with the pre-seeded intent injected via `with_initial_pending_intents`.
        // `on_start` receives it as `initial_pending_intents`, populates the flat
        // `pending_intents` field, and calls `replay_and_redispatch` — exercising
        // the supervised-restart re-dispatch path.
        let _proxy = SagaProps::<InertSaga>::new(
            saga_id.clone(),
            Arc::clone(&inner_store) as Arc<dyn EventStore>,
            InertSaga::default,
        )
        .with_initial_pending_intents(initial_intents)
        .with_codec(Arc::new(JsonCodec) as Arc<dyn ErasedCodec<SagaEvt>>)
        .with_subscription(
            Arc::clone(&upstream_store),
            Arc::new(JsonCodec) as Arc<dyn ErasedCodec<UpstreamEvt>>,
            SequenceCursor::Stream {
                key: "no-such-upstream".to_owned(),
                after: 0,
            },
            route_fn,
        )
        .spawn(&ps)
        .await;

        // Wait for the supervised-restart re-dispatch to produce TellAcked.
        let saga_store: Arc<dyn EventStore> = Arc::clone(&inner_store) as Arc<dyn EventStore>;
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            let events = load_outbox_events(&saga_store, &saga_id).await;
            let acked = events
                .iter()
                .filter(|e| e.event_type.as_str() == "nitinol.saga.outbox.tell_acked")
                .count();
            if acked >= 1 {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "timed out waiting for TellAcked on supervised-restart re-dispatch; \
                 events={:?}",
                events
                    .iter()
                    .map(|e| e.event_type.as_str())
                    .collect::<Vec<_>>()
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        let events = load_outbox_events(&saga_store, &saga_id).await;
        let acked = events
            .iter()
            .filter(|e| e.event_type.as_str() == "nitinol.saga.outbox.tell_acked")
            .count();
        let failed = events
            .iter()
            .filter(|e| e.event_type.as_str() == "nitinol.saga.outbox.tell_failed")
            .count();

        assert_eq!(
            acked, 1,
            "supervised-restart re-dispatch must produce exactly one TellAcked"
        );
        assert_eq!(
            failed, 0,
            "supervised-restart re-dispatch must NOT produce TellFailed when \
             the in-memory intent is available"
        );
    }

    // -----------------------------------------------------------------------
    // Test 2 — intent survives when terminal append fails
    //
    // Contract: when the outbox executor calls `AppendTerminalAndClaim` but
    // the store append fails (returns `false`), the executor must NOT send
    // `OutboxTerminalSettled`.  Consequently `pending_intents` is NOT cleared,
    // leaving the entry intact for the next supervised restart.
    //
    // Observable consequence: no `TellAcked` and no `TellFailed` is written to
    // the store after the executor runs — the outbox stream remains in the
    // pending state.
    // -----------------------------------------------------------------------

    /// When `AppendTerminalAndClaim` fails (store error), the executor exits
    /// without sending `OutboxTerminalSettled`, so the `pending_intents` entry
    /// is kept for the next supervised restart.  The observable effect is that
    /// the store contains neither `TellAcked` nor `TellFailed` after the
    /// executor completes.
    #[tokio::test]
    async fn replay_keeps_pending_intent_when_terminal_append_fails() {
        let mock = MockAggregateProxy::<MarkerAggregate>::new();
        let ps = ProcessSystem::new().await;

        // Use an inner InMemoryEventStore for load, wrapped so that ALL
        // appends fail.  This ensures that `AppendTerminalAndClaim` cannot
        // write a terminal marker, exercising the "executor sees false, exits
        // without OutboxTerminalSettled" path.
        let inner_store = Arc::new(InMemoryEventStore::default());
        let saga_id = SagaId::new("supervised-replay-keeps-intent-unit-1");

        // Seed TellRequested BEFORE wrapping with FailAppendStore.
        seed_tell_requested(&inner_store, &saga_id, 1, 1).await;

        let fail_store = FailAppendStore::wrap(Arc::clone(&inner_store));

        let intent = TellIntent::new::<MarkerAggregate, MarkerCmd, _>(mock.clone(), MarkerCmd);
        let mut initial_intents = HashMap::new();
        initial_intents.insert(1u64, intent);

        let upstream_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
        let routed = saga_id.clone();
        let route_fn = move |_: &UpstreamEvt| Some(routed.clone());

        // Spawn saga against the fail-all-appends store.  The executor will
        // call `ask(AppendTerminalAndClaim)`, which uses `fail_store.append`,
        // gets `false` back, and exits WITHOUT sending `OutboxTerminalSettled`.
        let _proxy = SagaProps::<InertSaga>::new(
            saga_id.clone(),
            Arc::clone(&fail_store),
            InertSaga::default,
        )
        .with_initial_pending_intents(initial_intents)
        .with_codec(Arc::new(JsonCodec) as Arc<dyn ErasedCodec<SagaEvt>>)
        .with_subscription(
            Arc::clone(&upstream_store),
            Arc::new(JsonCodec) as Arc<dyn ErasedCodec<UpstreamEvt>>,
            SequenceCursor::Stream {
                key: "no-such-upstream".to_owned(),
                after: 0,
            },
            route_fn,
        )
        .spawn(&ps)
        .await;

        // Give the executor ample time to run and (fail to) append terminal.
        tokio::time::sleep(Duration::from_millis(500)).await;

        // The inner store should still have exactly the seeded TellRequested
        // and NOTHING else — no TellAcked, no TellFailed.
        let events =
            load_outbox_events(&(Arc::clone(&inner_store) as Arc<dyn EventStore>), &saga_id).await;
        let acked = events
            .iter()
            .filter(|e| e.event_type.as_str() == "nitinol.saga.outbox.tell_acked")
            .count();
        let failed = events
            .iter()
            .filter(|e| e.event_type.as_str() == "nitinol.saga.outbox.tell_failed")
            .count();

        assert_eq!(
            acked, 0,
            "terminal append failure must NOT produce TellAcked; \
             pending_intents entry is kept for next restart"
        );
        assert_eq!(
            failed, 0,
            "terminal append failure must NOT produce TellFailed; \
             pending_intents entry is kept for next restart"
        );
    }
}
