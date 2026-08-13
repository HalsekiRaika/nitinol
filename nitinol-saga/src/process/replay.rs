use std::collections::HashMap;
use std::sync::Arc;

use futures_util::StreamExt;
use nitinol_eventsource::codec::ErasedCodec;
use nitinol_persistence::store::EventStore;
use nitinol_persistence::{LoadQuery, LoadedEvent};
use nitinol_runtime::process::ProcessContext;

use crate::dead_letter::{
    enqueue_dead_letter, EnqueueOutcome, EnqueuePolicy, SagaFailure, SourceContext,
};
use crate::effect::TellIntent;
use crate::id::SagaId;
use crate::journal::{ActiveSchedule, JournalState, PendingTell};
use crate::outbox::RetryPolicy;
use crate::outbox::{OutboxAppender, TellOutcome};
use crate::persisted::{SagaPersisted, SagaPersistedDecodeError};
use crate::process::outbox_executor::spawn_outbox_executor;
use crate::process::saga_process::{SagaProcess, TellState};
use crate::saga::Saga;

/// Rebuilds a [`TellIntent`] from the serialized command a `TellRequested`
/// carried, so a tell pending at crash time can be re-dispatched once the
/// OS process restarts and the in-memory intent is gone.
///
/// Returns `None` when the payload cannot be reconstructed; replay then
/// appends a synthetic `TellFailed` instead of silently dropping the tell.
type CrashRestartFactory<'a> = &'a (dyn Fn(&[u8]) -> Option<TellIntent> + Send + Sync);

/// Outcome of replaying the saga's own event stream on `on_start`.
pub(crate) struct ReplayOutcome {
    /// `tell_id`s whose terminal marker is `TellFailed` (seen during replay or
    /// synthesised for un-redispatchable pending tells).
    pub(crate) failed: Vec<u64>,
    /// `true` when the stream carries a durable `Ended` marker — the saga
    /// terminated in a previous incarnation and must not be revived.
    pub(crate) ended: bool,
    /// Schedules still pending after replay, to be re-registered with the
    /// scheduler.  Empty when the saga has ended.
    pub(crate) active_schedules: Vec<ActiveSchedule>,
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn replay_and_redispatch<S: Saga>(
    saga_id: &SagaId,
    state: &mut S,
    codec: &dyn ErasedCodec<S::Event>,
    store: &Arc<dyn EventStore>,
    sequence: &mut u64,
    tell_states: &mut HashMap<u64, TellState>,
    crash_restart_factory: Option<CrashRestartFactory<'_>>,
    retry_policy: RetryPolicy,
    enqueue_policy: &dyn EnqueuePolicy,
    ctx: &mut ProcessContext<SagaProcess<S>>,
) -> Result<ReplayOutcome, ()> {
    let journal = match load_journal(saga_id, state, codec, store, *sequence).await {
        Some(j) => j,
        // store.load() or stream iteration failed — the terminated state is
        // unknown.  Fail safe (not fail-open): return Err so on_start stops
        // the process without subscribing.
        None => return Err(()),
    };
    *sequence = journal.sequence;
    // When the stream carries a durable `Ended` marker the saga terminated
    // in a previous incarnation.  Skip `redispatch_pending` entirely so that any
    // in-flight `TellRequested` entries are NOT re-executed after termination.
    // Ended sagas also skip schedule re-registration.
    if journal.ended {
        return Ok(ReplayOutcome {
            failed: journal.failed_tell_ids,
            ended: true,
            active_schedules: Vec::new(),
        });
    }
    let active_schedules: Vec<ActiveSchedule> = journal.active_schedules.into_values().collect();
    let mut failed = journal.failed_tell_ids;
    let synthetic_failed = redispatch_pending(
        saga_id,
        store,
        sequence,
        journal.pending_tells,
        tell_states,
        crash_restart_factory,
        retry_policy,
        enqueue_policy,
        ctx,
    )
    .await?;
    failed.extend(synthetic_failed);
    Ok(ReplayOutcome {
        failed,
        ended: false,
        active_schedules,
    })
}

/// Load the saga's own stream from `from_sequence + 1` and fold it into a
/// [`JournalState`], applying the domain events it hands back to `state` in
/// stream order.
///
/// Returns `None` when the stream could not be read to its end; the caller then
/// treats the terminated state as unknown.
async fn load_journal<S: Saga>(
    saga_id: &SagaId,
    state: &mut S,
    codec: &dyn ErasedCodec<S::Event>,
    store: &Arc<dyn EventStore>,
    from_sequence: u64,
) -> Option<JournalState> {
    let query = LoadQuery {
        stream_key: Some(saga_id.as_str().to_owned()),
        from_stream_sequence: Some(from_sequence + 1),
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
    let mut journal = JournalState::new(from_sequence);
    while let Some(item) = stream.next().await {
        let loaded = match item {
            Ok(ev) => ev,
            Err(e) => {
                tracing::error!(error = ?e, "saga event store stream error during replay");
                return None;
            }
        };
        fold_loaded::<S>(loaded, state, codec, &mut journal);
    }
    Some(journal)
}

/// Classify one loaded record and fold it, applying a domain event to the
/// saga's state.  An undecodable record is logged and skipped, so a single
/// poisoned payload does not abort the replay.
fn fold_loaded<S: Saga>(
    loaded: LoadedEvent,
    state: &mut S,
    codec: &dyn ErasedCodec<S::Event>,
    journal: &mut JournalState,
) {
    journal.observe_sequence(loaded.sequence);
    match SagaPersisted::classify(loaded.event_type, &loaded.payload, codec) {
        Ok(persisted) => {
            if let Some(event) = journal.fold(persisted) {
                state.apply(event);
            }
        }
        Err(SagaPersistedDecodeError::Outbox(e)) => {
            tracing::error!(error = %e, "saga outbox marker decode failed; skipping event");
        }
        Err(SagaPersistedDecodeError::Schedule(e)) => {
            tracing::error!(error = %e, "saga schedule marker decode failed; skipping event");
        }
        Err(SagaPersistedDecodeError::DeadLetter(e)) => {
            tracing::error!(error = %e, "saga dead letter decode failed; skipping event");
        }
        Err(SagaPersistedDecodeError::Domain(e)) => {
            tracing::error!(error = %e, "saga event decode failed; skipping event");
        }
    }
}

fn intent_of(state: Option<&TellState>) -> Option<TellIntent> {
    match state {
        Some(TellState::Pending(intent)) | Some(TellState::AppendFailed(intent)) => {
            Some(intent.clone())
        }
        _ => None,
    }
}

#[allow(clippy::too_many_arguments)]
async fn redispatch_pending<S: Saga>(
    saga_id: &SagaId,
    store: &Arc<dyn EventStore>,
    sequence: &mut u64,
    pending: HashMap<u64, PendingTell>,
    tell_states: &mut HashMap<u64, TellState>,
    crash_restart_factory: Option<CrashRestartFactory<'_>>,
    retry_policy: RetryPolicy,
    enqueue_policy: &dyn EnqueuePolicy,
    ctx: &mut ProcessContext<SagaProcess<S>>,
) -> Result<Vec<u64>, ()> {
    let mut synthetic_failed: Vec<u64> = Vec::new();
    for (tell_id, pending_tell) in pending {
        let resolved = if let Some(intent) = intent_of(tell_states.get(&tell_id)) {
            tracing::debug!(
                tell_id,
                "saga replay: supervised restart — re-dispatching pending tell"
            );
            Some(intent)
        } else if let (Some(factory), Some(payload)) =
            (crash_restart_factory, pending_tell.crash_restart.as_deref())
        {
            match factory(payload) {
                Some(reconstructed) => {
                    tracing::debug!(
                        tell_id,
                        "saga replay: crash restart — reconstructed TellIntent from payload"
                    );
                    tell_states.insert(tell_id, TellState::Pending(reconstructed.clone()));
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
            tracing::warn!(
                tell_id,
                has_factory = crash_restart_factory.is_some(),
                has_payload = pending_tell.crash_restart.is_some(),
                "saga replay: pending TellRequested cannot be re-dispatched \
                 (configure crash-restart factory and crash-restart payload to \
                 enable crash-restart re-dispatch); appending synthetic TellFailed"
            );
            None
        };

        if let Some(intent) = resolved {
            if matches!(tell_states.get(&tell_id), Some(TellState::AppendFailed(_))) {
                tell_states.insert(tell_id, TellState::Pending(intent.clone()));
            }
            spawn_outbox_executor(ctx, intent, tell_id, retry_policy.clone()).await;
        } else {
            *sequence += 1;
            let claimed = *sequence;
            let appended = OutboxAppender::append_terminal(
                store,
                saga_id,
                claimed,
                TellOutcome::Failed,
                tell_id,
            )
            .await;
            if appended {
                synthetic_failed.push(tell_id);
                // Write a DLQ entry only when the target stream key is recoverable.
                // Legacy streams written before `TellRequested.target` (proto field 3)
                // was added have `target == None`; emitting `TellFailed` with an empty
                // target is not allowed, so those entries are skipped.
                // The durable outbox marker already records the failure.
                if let Some(target) = pending_tell.target {
                    let message = pending_tell.crash_restart.unwrap_or_default();
                    let outcome = enqueue_dead_letter(
                        store,
                        saga_id,
                        sequence,
                        enqueue_policy,
                        SagaFailure::TellFailed { target, message },
                        SourceContext::without_upstream(),
                    )
                    .await;
                    // When the DLQ append fails the synthetic TellFailed is
                    // already written to the outbox, but the DLQ entry is missing.
                    // Stop so the process is not considered cleanly started; a
                    // supervised restart will re-evaluate from the durable markers.
                    if matches!(outcome, EnqueueOutcome::AppendFailed) {
                        tracing::error!(
                            tell_id,
                            "saga replay: DLQ append failed for synthetic TellFailed; \
                             stopping process"
                        );
                        return Err(());
                    }
                } else {
                    tracing::warn!(
                        tell_id,
                        "saga replay: legacy TellRequested has no target field; \
                         DLQ entry skipped — outbox marker is the durable failure record"
                    );
                }
            } else {
                *sequence -= 1;
            }
        }
    }

    Ok(synthetic_failed)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::pin::Pin;
    use std::sync::atomic::{AtomicUsize, Ordering};
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
    use nitinol_persistence::{AppendOutcome, AppendingEvent, EventType, Family, LoadQuery};
    use nitinol_persistence::{LoadedEvent, TypeName};
    use nitinol_runtime::ProcessSystem;

    use nitinol_eventsource::SystemEvent;

    use crate::outbox::{is_outbox_event_type, OutboxAppender, OutboxEvent};
    use crate::{Saga, SagaContext, SagaEffect, SagaId, SagaProps, TellIntent};

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

    #[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
    struct MarkerEvent;

    impl Event for MarkerEvent {
        const EVENT_TYPE: EventType =
            EventType::new(Family::new("replay_unit_test"), TypeName::new("Marker"));
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

    #[derive(Clone, Debug, Serialize, Deserialize)]
    struct UpstreamEvt;

    impl Event for UpstreamEvt {
        const EVENT_TYPE: EventType =
            EventType::new(Family::new("replay_unit_test"), TypeName::new("Upstream"));
    }

    #[derive(Clone, Debug, Serialize, Deserialize)]
    struct SagaEvt;

    impl Event for SagaEvt {
        const EVENT_TYPE: EventType =
            EventType::new(Family::new("replay_unit_test"), TypeName::new("SagaEvent"));
    }

    #[derive(Default)]
    struct InertSaga;

    #[async_trait]
    impl Saga for InertSaga {
        type SubscribedEvent = UpstreamEvt;
        type Event = SagaEvt;
        type State = ();
        type ScheduledMessage = ();
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

    struct FailAppendStore {
        inner: Arc<InMemoryEventStore>,
    }

    impl FailAppendStore {
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
            let events: Vec<LoadedEvent> = self.inner.load(query).await?.try_collect().await?;
            let stream: Pin<
                Box<dyn Stream<Item = Result<LoadedEvent, LoadError>> + Send + 'static>,
            > = Box::pin(futures_util::stream::iter(events.into_iter().map(Ok)));
            Ok(stream)
        }
    }

    /// A store that always fails `load()`.  Used to simulate an unreachable event
    /// store during replay, verifying the fail-safe guard against reviving a
    /// possibly-terminated saga.
    struct FailLoadStore;

    #[async_trait]
    impl EventStore for FailLoadStore {
        async fn append(
            &self,
            _key: &str,
            _events: Vec<AppendingEvent>,
        ) -> Result<AppendOutcome, AppendError> {
            Ok(AppendOutcome {
                assigned_sequences: vec![],
                stream_version: 0,
            })
        }

        async fn load(&self, _query: LoadQuery) -> Result<EventStream<'_>, LoadError> {
            Err(LoadError::Backend("injected: load always fails".into()))
        }
    }

    /// A saga that counts every `handle` call.  Used to assert that the upstream
    /// subscription is never wired when replay fails.
    #[derive(Default)]
    struct CountingInertSaga {
        handle_count: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl Saga for CountingInertSaga {
        type SubscribedEvent = UpstreamEvt;
        type Event = SagaEvt;
        type State = ();
        type ScheduledMessage = ();
        type Error = std::convert::Infallible;

        fn apply(&mut self, _event: SagaEvt) {}

        async fn handle(
            &mut self,
            _event: UpstreamEvt,
            _ctx: &mut SagaContext,
        ) -> Result<SagaEffect<SagaEvt>, Self::Error> {
            self.handle_count.fetch_add(1, Ordering::SeqCst);
            Ok(SagaEffect::None)
        }
    }

    async fn seed_tell_requested(
        store: &Arc<InMemoryEventStore>,
        saga_id: &SagaId,
        sequence: u64,
        tell_id: u64,
    ) {
        let event = OutboxAppender::build_tell_requested(
            sequence,
            tell_id,
            None,
            "",
            jiff::Timestamp::now(),
        );
        store
            .append(saga_id.as_str(), vec![event])
            .await
            .expect("seed TellRequested must succeed");
    }

    async fn seed_ended(store: &Arc<InMemoryEventStore>, saga_id: &SagaId, sequence: u64) {
        let store_dyn: Arc<dyn EventStore> = Arc::clone(store) as Arc<dyn EventStore>;
        let ok = OutboxAppender::append_ended(&store_dyn, saga_id, sequence).await;
        assert!(ok, "seed Ended must succeed");
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

    fn count_outbox(events: &[LoadedEvent], pred: impl Fn(&OutboxEvent) -> bool) -> usize {
        events
            .iter()
            .filter(|e| {
                is_outbox_event_type(e.event_type)
                    && matches!(OutboxEvent::decode(&e.payload), Ok(ref m) if pred(m))
            })
            .count()
    }

    #[tokio::test]
    async fn unacked_tell_requested_with_pending_intent_yields_tell_acked_on_replay() {
        let mock = MockAggregateProxy::<MarkerAggregate>::new();
        let ps = ProcessSystem::new().await;

        let inner_store = Arc::new(InMemoryEventStore::default());
        let saga_id = SagaId::new("supervised-replay-acked-unit-1");

        seed_tell_requested(&inner_store, &saga_id, 1, 1).await;

        use crate::process::saga_process::TellState;
        let intent = TellIntent::new::<MarkerAggregate, MarkerCmd, _>(mock.clone(), MarkerCmd);
        let mut initial_tell_states = HashMap::new();
        initial_tell_states.insert(1u64, TellState::Pending(intent));

        let upstream_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
        let routed = saga_id.clone();
        let route_fn = move |_: &UpstreamEvt| Some(routed.clone());

        let _proxy = SagaProps::<InertSaga>::new(
            saga_id.clone(),
            Arc::clone(&inner_store) as Arc<dyn EventStore>,
            InertSaga::default,
        )
        .with_initial_tell_states(initial_tell_states)
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

        let saga_store: Arc<dyn EventStore> = Arc::clone(&inner_store) as Arc<dyn EventStore>;
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            let events = load_outbox_events(&saga_store, &saga_id).await;
            let acked = count_outbox(&events, |m| matches!(m, OutboxEvent::TellAcked(_)));
            if acked >= 1 {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "timed out waiting for TellAcked on supervised-restart re-dispatch; \
                 events={:?}",
                events
                    .iter()
                    .map(|e| e.event_type.to_string())
                    .collect::<Vec<_>>()
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        let events = load_outbox_events(&saga_store, &saga_id).await;
        let acked = count_outbox(&events, |m| matches!(m, OutboxEvent::TellAcked(_)));
        let failed = count_outbox(&events, |m| matches!(m, OutboxEvent::TellFailed(_)));

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

    #[tokio::test]
    async fn replay_keeps_pending_intent_when_terminal_append_fails() {
        let mock = MockAggregateProxy::<MarkerAggregate>::new();
        let ps = ProcessSystem::new().await;

        let inner_store = Arc::new(InMemoryEventStore::default());
        let saga_id = SagaId::new("supervised-replay-keeps-intent-unit-1");

        seed_tell_requested(&inner_store, &saga_id, 1, 1).await;

        let fail_store = FailAppendStore::wrap(Arc::clone(&inner_store));

        use crate::process::saga_process::TellState;
        let intent = TellIntent::new::<MarkerAggregate, MarkerCmd, _>(mock.clone(), MarkerCmd);
        let mut initial_tell_states = HashMap::new();
        initial_tell_states.insert(1u64, TellState::Pending(intent));

        let upstream_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
        let routed = saga_id.clone();
        let route_fn = move |_: &UpstreamEvt| Some(routed.clone());

        let _proxy = SagaProps::<InertSaga>::new(
            saga_id.clone(),
            Arc::clone(&fail_store),
            InertSaga::default,
        )
        .with_initial_tell_states(initial_tell_states)
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

        tokio::time::sleep(Duration::from_millis(500)).await;

        let events =
            load_outbox_events(&(Arc::clone(&inner_store) as Arc<dyn EventStore>), &saga_id).await;
        let acked = count_outbox(&events, |m| matches!(m, OutboxEvent::TellAcked(_)));
        let failed = count_outbox(&events, |m| matches!(m, OutboxEvent::TellFailed(_)));

        assert_eq!(
            acked, 0,
            "terminal append failure must NOT produce TellAcked; \
             tell state entry is preserved as AppendFailed for next restart"
        );
        assert_eq!(
            failed, 0,
            "terminal append failure must NOT produce TellFailed; \
             tell state entry is preserved as AppendFailed for next restart"
        );
    }

    #[tokio::test]
    async fn append_failed_tell_is_redispatched_on_supervised_restart() {
        use crate::process::saga_process::TellState;

        let mock = MockAggregateProxy::<MarkerAggregate>::new();
        let ps = ProcessSystem::new().await;

        let store = Arc::new(InMemoryEventStore::default());
        let saga_id = SagaId::new("append-failed-redispatch-regression-1");

        seed_tell_requested(&store, &saga_id, 1, 1).await;

        let intent = TellIntent::new::<MarkerAggregate, MarkerCmd, _>(mock.clone(), MarkerCmd);
        let mut initial_tell_states = HashMap::new();
        initial_tell_states.insert(1u64, TellState::AppendFailed(intent));

        let upstream_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
        let routed = saga_id.clone();
        let route_fn = move |_: &UpstreamEvt| Some(routed.clone());

        let _saga_proxy = SagaProps::<InertSaga>::new(
            saga_id.clone(),
            Arc::clone(&store) as Arc<dyn EventStore>,
            InertSaga::default,
        )
        .with_initial_tell_states(initial_tell_states)
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

        let saga_store: Arc<dyn EventStore> = Arc::clone(&store) as Arc<dyn EventStore>;
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            let events = load_outbox_events(&saga_store, &saga_id).await;
            let acked = count_outbox(&events, |m| matches!(m, OutboxEvent::TellAcked(_)));
            if acked >= 1 {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "timed out waiting for TellAcked on AppendFailed supervised-restart \
                 re-dispatch; events={:?}",
                events
                    .iter()
                    .map(|e| e.event_type.to_string())
                    .collect::<Vec<_>>()
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        let events = load_outbox_events(&saga_store, &saga_id).await;
        let acked = count_outbox(&events, |m| matches!(m, OutboxEvent::TellAcked(_)));
        let failed = count_outbox(&events, |m| matches!(m, OutboxEvent::TellFailed(_)));

        assert_eq!(
            acked, 1,
            "AppendFailed supervised-restart re-dispatch must produce exactly one TellAcked"
        );
        assert_eq!(
            failed, 0,
            "AppendFailed supervised-restart re-dispatch must NOT produce TellFailed \
             when the in-memory intent is available"
        );
    }

    /// Regression test: when the saga stream carries both a `TellRequested`
    /// and a durable `Ended` marker, `replay_and_redispatch` must skip
    /// `redispatch_pending` entirely.  Even with a live in-memory intent
    /// available, no `TellAcked` or `TellFailed` must be appended.
    #[tokio::test]
    async fn pending_tell_is_not_redispatched_when_ended_marker_present() {
        let mock = MockAggregateProxy::<MarkerAggregate>::new();
        let ps = ProcessSystem::new().await;

        let inner_store = Arc::new(InMemoryEventStore::default());
        let saga_id = SagaId::new("ended-no-redispatch-regression-1");

        // seq=1: pending tell; seq=2: durable Ended marker
        seed_tell_requested(&inner_store, &saga_id, 1, 1).await;
        seed_ended(&inner_store, &saga_id, 2).await;

        // Provide an in-memory intent so redispatch would succeed if (incorrectly) called.
        use crate::process::saga_process::TellState;
        let intent = TellIntent::new::<MarkerAggregate, MarkerCmd, _>(mock.clone(), MarkerCmd);
        let mut initial_tell_states = HashMap::new();
        initial_tell_states.insert(1u64, TellState::Pending(intent));

        let upstream_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
        let routed = saga_id.clone();
        let route_fn = move |_: &UpstreamEvt| Some(routed.clone());

        let _proxy = SagaProps::<InertSaga>::new(
            saga_id.clone(),
            Arc::clone(&inner_store) as Arc<dyn EventStore>,
            InertSaga::default,
        )
        .with_initial_tell_states(initial_tell_states)
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

        // Give ample time for any (incorrect) redispatch to append a terminal marker.
        tokio::time::sleep(Duration::from_millis(500)).await;

        let events =
            load_outbox_events(&(Arc::clone(&inner_store) as Arc<dyn EventStore>), &saga_id).await;
        let acked = count_outbox(&events, |m| matches!(m, OutboxEvent::TellAcked(_)));
        let failed = count_outbox(&events, |m| matches!(m, OutboxEvent::TellFailed(_)));

        assert_eq!(
            acked, 0,
            "pending tell must NOT be redispatched when Ended marker is present \
             (regression)"
        );
        assert_eq!(
            failed, 0,
            "pending tell must NOT produce synthetic TellFailed when Ended marker \
             is present (regression)"
        );
    }

    /// A store that succeeds the **first** `append` call (routing through the
    /// inner `InMemoryEventStore`) and fails every subsequent append.  Used to
    /// allow `OutboxAppender::append_terminal` to succeed while making the DLQ
    /// enqueue that follows it fail — so the `AppendFailed` path in
    /// `redispatch_pending` is exercised.
    struct FailSecondAppendStore {
        inner: Arc<InMemoryEventStore>,
        append_count: std::sync::atomic::AtomicUsize,
    }

    impl FailSecondAppendStore {
        fn into_store(inner: Arc<InMemoryEventStore>) -> Arc<dyn EventStore> {
            Arc::new(Self {
                inner,
                append_count: std::sync::atomic::AtomicUsize::new(0),
            })
        }
    }

    #[async_trait]
    impl EventStore for FailSecondAppendStore {
        async fn append(
            &self,
            key: &str,
            events: Vec<AppendingEvent>,
        ) -> Result<AppendOutcome, AppendError> {
            let count = self
                .append_count
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if count == 0 {
                self.inner.append(key, events).await
            } else {
                Err(AppendError::Backend(
                    "injected: second+ appends fail".into(),
                ))
            }
        }

        async fn load(&self, query: LoadQuery) -> Result<EventStream<'_>, LoadError> {
            let events: Vec<LoadedEvent> = self.inner.load(query).await?.try_collect().await?;
            let stream: Pin<
                Box<dyn Stream<Item = Result<LoadedEvent, LoadError>> + Send + 'static>,
            > = Box::pin(futures_util::stream::iter(events.into_iter().map(Ok)));
            Ok(stream)
        }
    }

    /// Regression test: when `OutboxAppender::append_terminal`
    /// succeeds for a synthetic TellFailed during replay but the subsequent DLQ
    /// enqueue fails, `redispatch_pending` must return `Err(())`,
    /// `replay_and_redispatch` must propagate it, and `on_start` must stop the
    /// process without wiring the upstream subscription — a DLQ append failure
    /// must not silently treat the failure as processed.
    ///
    /// Concretely: even if the upstream store has a routed event, `Saga::handle`
    /// must never be called when the replay DLQ append fails.
    #[tokio::test]
    async fn replay_dlq_append_failure_stops_saga_and_prevents_upstream_subscription() {
        let ps = ProcessSystem::new().await;

        let inner_store = Arc::new(InMemoryEventStore::default());
        let saga_id = SagaId::new("replay-dlq-fail-regression-1");

        // Seed a TellRequested with a non-empty target so the synthetic TellFailed
        // path reaches the DLQ enqueue.  (Empty target → legacy path, no DLQ.)
        let seed_event = OutboxAppender::build_tell_requested(
            1,
            1,
            None, // no crash-restart payload → saga has no factory → synthetic TellFailed
            "some-target-aggregate",
            jiff::Timestamp::now(),
        );
        inner_store
            .append(saga_id.as_str(), vec![seed_event])
            .await
            .expect("seed TellRequested must succeed");

        // First append (OutboxAppender::append_terminal) succeeds.
        // Second append (DLQ enqueue) fails → replay_and_redispatch returns Err(()).
        let fail_second: Arc<dyn EventStore> =
            FailSecondAppendStore::into_store(Arc::clone(&inner_store));

        let handle_count = Arc::new(AtomicUsize::new(0));

        // Pre-seed an upstream event that would be delivered if the saga subscribed.
        let upstream_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
        let upstream_ev = nitinol_persistence::AppendingEvent {
            sequence: 1,
            event_type: UpstreamEvt::EVENT_TYPE,
            payload: serde_json::to_vec(&UpstreamEvt)
                .map(Bytes::from)
                .expect("encode UpstreamEvt"),
            occurred_at: jiff::Timestamp::now(),
        };
        upstream_store
            .append("replay-dlq-fail-upstream", vec![upstream_ev])
            .await
            .expect("upstream append must succeed");

        let handle_count_clone = Arc::clone(&handle_count);
        let routed = saga_id.clone();
        let route_fn = move |_: &UpstreamEvt| Some(routed.clone());

        let _proxy = SagaProps::<CountingInertSaga>::new(
            saga_id.clone(),
            Arc::clone(&fail_second),
            move || CountingInertSaga {
                handle_count: Arc::clone(&handle_count_clone),
            },
        )
        .with_codec(Arc::new(JsonCodec) as Arc<dyn ErasedCodec<SagaEvt>>)
        .with_subscription(
            Arc::clone(&upstream_store),
            Arc::new(JsonCodec) as Arc<dyn ErasedCodec<UpstreamEvt>>,
            SequenceCursor::Stream {
                key: "replay-dlq-fail-upstream".to_owned(),
                after: 0,
            },
            route_fn,
        )
        .spawn(&ps)
        .await;

        // Give ample time for any (incorrect) subscription and event delivery.
        tokio::time::sleep(Duration::from_millis(500)).await;

        assert_eq!(
            handle_count.load(Ordering::SeqCst),
            0,
            "Saga::handle must never be called when replay DLQ append fails — \
             the process must stop itself instead of subscribing"
        );
    }

    /// Regression test: when `EventStore::load` fails during replay the
    /// terminated state is unknown.  `replay_and_redispatch` must return `Err`
    /// so `on_start` stops the process instead of subscribing to upstream events
    /// with an indeterminate terminated state (fail-safe, not fail-open).
    ///
    /// Concretely: even if the upstream store already has a routed event,
    /// `Saga::handle` must never be called when the saga's own store fails to
    /// load.
    #[tokio::test]
    async fn saga_stops_and_skips_subscription_when_event_store_load_fails() {
        let ps = ProcessSystem::new().await;

        let saga_id = SagaId::new("air-004-load-fail-regression-1");
        let handle_count = Arc::new(AtomicUsize::new(0));

        let handle_count_clone = Arc::clone(&handle_count);
        let upstream_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
        let routed = saga_id.clone();
        let route_fn = move |_: &UpstreamEvt| Some(routed.clone());

        // Pre-seed an upstream event so the subscription would deliver it if
        // the saga were (incorrectly) to subscribe despite the load failure.
        let upstream_ev = nitinol_persistence::AppendingEvent {
            sequence: 1,
            event_type: UpstreamEvt::EVENT_TYPE,
            payload: serde_json::to_vec(&UpstreamEvt)
                .map(Bytes::from)
                .expect("encode UpstreamEvt"),
            occurred_at: jiff::Timestamp::now(),
        };
        upstream_store
            .append("air-004-upstream", vec![upstream_ev])
            .await
            .expect("upstream append must succeed");

        let _proxy = SagaProps::<CountingInertSaga>::new(
            saga_id.clone(),
            // Saga store: load always fails → replay_and_redispatch returns Err.
            Arc::new(FailLoadStore) as Arc<dyn EventStore>,
            move || CountingInertSaga {
                handle_count: Arc::clone(&handle_count_clone),
            },
        )
        .with_codec(Arc::new(JsonCodec) as Arc<dyn ErasedCodec<SagaEvt>>)
        .with_subscription(
            Arc::clone(&upstream_store),
            Arc::new(JsonCodec) as Arc<dyn ErasedCodec<UpstreamEvt>>,
            SequenceCursor::Stream {
                key: "air-004-upstream".to_owned(),
                after: 0,
            },
            route_fn,
        )
        .spawn(&ps)
        .await;

        // Give ample time for any (incorrect) subscription and event delivery.
        tokio::time::sleep(Duration::from_millis(500)).await;

        assert_eq!(
            handle_count.load(Ordering::SeqCst),
            0,
            "Saga::handle must never be called when EventStore::load fails during \
             replay — the process must stop itself instead of subscribing"
        );
    }
}
