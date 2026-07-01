use std::collections::HashMap;
use std::sync::Arc;

use bytes::Bytes;
use futures_util::StreamExt;
use nitinol_eventsource::codec::ErasedCodec;
use nitinol_persistence::store::EventStore;
use nitinol_persistence::{LoadQuery, LoadedEvent};
use nitinol_runtime::process::ProcessContext;

use crate::effect::TellIntent;
use crate::id::SagaId;
use crate::outbox::RetryPolicy;
use crate::outbox::{OutboxAppender, OutboxMessage, TellOutcome};
use crate::process::outbox_executor::spawn_outbox_executor;
use crate::process::saga_process::{SagaProcess, TellState};
use crate::saga::Saga;

#[allow(clippy::too_many_arguments)]
pub(crate) async fn replay_and_redispatch<S: Saga>(
    saga_id: &SagaId,
    state: &mut S,
    codec: &dyn ErasedCodec<S::Event>,
    store: &Arc<dyn EventStore>,
    sequence: &mut u64,
    tell_states: &mut HashMap<u64, TellState>,
    crash_restart_factory: Option<&(dyn Fn(&[u8]) -> Option<TellIntent> + Send + Sync)>,
    retry_policy: RetryPolicy,
    ctx: &mut ProcessContext<SagaProcess<S>>,
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
        tell_states,
        crash_restart_factory,
        retry_policy,
        ctx,
    )
    .await;
    failed.extend(synthetic_failed);
    failed
}

struct ReplayScan {
    pending: HashMap<u64, Option<Bytes>>,
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
    let mut pending: HashMap<u64, Option<Bytes>> = HashMap::new();
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
    match OutboxMessage::classify(loaded.event_type, &loaded.payload) {
        Some(Ok(message)) => apply_outbox_message(message, pending, failed),
        Some(Err(e)) => {
            tracing::error!(error = %e, "saga outbox marker decode failed; skipping event");
        }
        None => match codec.decode(&loaded.payload) {
            Ok(event) => state.apply(event),
            Err(e) => {
                tracing::error!(error = %e, "saga event decode failed; skipping event");
            }
        },
    }
}

fn apply_outbox_message(
    message: OutboxMessage,
    pending: &mut HashMap<u64, Option<Bytes>>,
    failed: &mut Vec<u64>,
) {
    match message {
        OutboxMessage::TellRequested(m) => {
            pending.insert(m.tell_id, m.crash_restart.map(Bytes::from));
        }
        OutboxMessage::TellAcked(m) => {
            pending.remove(&m.tell_id);
        }
        OutboxMessage::TellFailed(m) => {
            pending.remove(&m.tell_id);
            failed.push(m.tell_id);
        }
        OutboxMessage::Scheduled(m) => {
            // Scheduled markers are durable but a replay no-op; there is nothing
            // to re-drive, so we only record that one was seen.
            tracing::trace!(
                at_unix_seconds = m.at_unix_seconds,
                "saga replay: scheduled marker (no-op)"
            );
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
    pending: HashMap<u64, Option<Bytes>>,
    tell_states: &mut HashMap<u64, TellState>,
    crash_restart_factory: Option<&(dyn Fn(&[u8]) -> Option<TellIntent> + Send + Sync)>,
    retry_policy: RetryPolicy,
    ctx: &mut ProcessContext<SagaProcess<S>>,
) -> Vec<u64> {
    let mut synthetic_failed: Vec<u64> = Vec::new();
    for (tell_id, crash_restart_payload) in pending {
        let resolved = if let Some(intent) = intent_of(tell_states.get(&tell_id)) {
            tracing::debug!(
                tell_id,
                "saga replay: supervised restart — re-dispatching pending tell"
            );
            Some(intent)
        } else if let (Some(factory), Some(payload)) =
            (crash_restart_factory, crash_restart_payload.as_deref())
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
                has_payload = crash_restart_payload.is_some(),
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
            } else {
                *sequence -= 1;
            }
        }
    }

    synthetic_failed
}

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
    use nitinol_persistence::{AppendingEvent, EventType, Family, LoadedEvent, TypeName};
    use nitinol_runtime::ProcessSystem;

    use crate::outbox::OutboxAppender;
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

    async fn seed_tell_requested(
        store: &Arc<InMemoryEventStore>,
        saga_id: &SagaId,
        sequence: u64,
        tell_id: u64,
    ) {
        // Seed through the real write path so the payload uses the framework's
        // prost `SystemEvent` codec (no crash-restart bytes).
        let event = OutboxAppender::build_tell_requested(
            sequence,
            tell_id,
            None,
            jiff::Timestamp::now(),
        );
        store
            .append(saga_id.as_str(), vec![event])
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
            let acked = events
                .iter()
                .filter(|e| e.event_type.to_string() == "nitinol.saga.outbox.tell_acked")
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
                    .map(|e| e.event_type.to_string())
                    .collect::<Vec<_>>()
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        let events = load_outbox_events(&saga_store, &saga_id).await;
        let acked = events
            .iter()
            .filter(|e| e.event_type.to_string() == "nitinol.saga.outbox.tell_acked")
            .count();
        let failed = events
            .iter()
            .filter(|e| e.event_type.to_string() == "nitinol.saga.outbox.tell_failed")
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
        let acked = events
            .iter()
            .filter(|e| e.event_type.to_string() == "nitinol.saga.outbox.tell_acked")
            .count();
        let failed = events
            .iter()
            .filter(|e| e.event_type.to_string() == "nitinol.saga.outbox.tell_failed")
            .count();

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

        let intent =
            TellIntent::new::<MarkerAggregate, MarkerCmd, _>(mock.clone(), MarkerCmd);
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
            let acked = events
                .iter()
                .filter(|e| e.event_type.to_string() == "nitinol.saga.outbox.tell_acked")
                .count();
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
        let acked = events
            .iter()
            .filter(|e| e.event_type.to_string() == "nitinol.saga.outbox.tell_acked")
            .count();
        let failed = events
            .iter()
            .filter(|e| e.event_type.to_string() == "nitinol.saga.outbox.tell_failed")
            .count();

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
}
