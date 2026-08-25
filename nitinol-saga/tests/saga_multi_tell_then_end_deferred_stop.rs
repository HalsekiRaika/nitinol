#[path = "common/helpers.rs"]
mod common;
use common::{outbox_kind_of, JsonCodec, OutboxKind};

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use futures_util::TryStreamExt;
use serde::{Deserialize, Serialize};

use nitinol_eventsource::{
    system::EventSourceSystem, Aggregate, AggregateProxy, Context, Decider, Effect, Event,
    SequenceCursor,
};
use nitinol_persistence::store::{EventStore, InMemoryEventStore};
use nitinol_persistence::{
    AggregateId, AppendingEvent, EventType, Family, LoadQuery, LoadedEvent, TypeName,
};
use nitinol_runtime::ProcessSystem;
use nitinol_saga::{Saga, SagaContext, SagaEffect, SagaId, SagaProps, TellIntent};

#[derive(Clone, Debug, Serialize, Deserialize)]
struct UpstreamTrigger {
    key: String,
}

impl Event for UpstreamTrigger {
    const EVENT_TYPE: EventType = EventType::new(
        Family::new("multi_tell_end"),
        TypeName::new("UpstreamTrigger"),
    );
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct SagaMarker {
    key: String,
}

impl Event for SagaMarker {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("multi_tell_end"), TypeName::new("SagaMarker"));
}

#[derive(Default)]
struct TargetAgg;

impl Aggregate for TargetAgg {
    type Event = SagaMarker;
    fn apply(&mut self, _event: SagaMarker) {}
}

#[derive(Clone, Serialize, Deserialize)]
struct TargetCmd {
    key: String,
}

#[async_trait]
impl Decider<TargetCmd> for TargetAgg {
    type Rejection = std::convert::Infallible;
    async fn decide(
        &self,
        cmd: TargetCmd,
        _ctx: &mut Context,
    ) -> Result<Effect<SagaMarker>, Self::Rejection> {
        Ok(Effect::persist(SagaMarker { key: cmd.key }))
    }
}

/// Correlation rule of [`TwoTellsThenEndSaga`]: each scenario runs one instance
/// against its own stores, so every `UpstreamTrigger` names that instance.
const TWO_TELLS_THEN_END_SAGA_ID: &str = "multi-tell-gapfree-saga";

struct TwoTellsThenEndSaga {
    target: AggregateProxy<TargetAgg>,
    handle_count: Arc<AtomicUsize>,
}

#[async_trait]
impl Saga for TwoTellsThenEndSaga {
    type SubscribedEvent = UpstreamTrigger;
    type Event = SagaMarker;
    type ScheduledMessage = ();
    type Error = std::convert::Infallible;

    fn correlate(_event: &Self::SubscribedEvent) -> Option<SagaId> {
        Some(SagaId::new(TWO_TELLS_THEN_END_SAGA_ID))
    }

    fn apply(&mut self, _event: SagaMarker) {}

    async fn handle(
        &mut self,
        event: UpstreamTrigger,
        _ctx: &mut SagaContext,
    ) -> Result<SagaEffect<SagaMarker>, Self::Error> {
        self.handle_count.fetch_add(1, Ordering::SeqCst);

        let intent_a = TellIntent::new(
            self.target.clone(),
            TargetCmd {
                key: format!("{}-A", event.key),
            },
        );
        let intent_b = TellIntent::new(
            self.target.clone(),
            TargetCmd {
                key: format!("{}-B", event.key),
            },
        );

        Ok(SagaEffect::persist(SagaMarker {
            key: event.key.clone(),
        })
        .combine(SagaEffect::tell_intent(intent_a))
        .combine(SagaEffect::tell_intent(intent_b))
        .then_end())
    }
}

async fn append_upstream(store: &Arc<dyn EventStore>, agg_id: &AggregateId, seq: u64, key: &str) {
    let payload = serde_json::to_vec(&UpstreamTrigger {
        key: key.to_owned(),
    })
    .map(Bytes::from)
    .expect("encode UpstreamTrigger");
    store
        .append(
            agg_id.as_str(),
            vec![AppendingEvent {
                sequence: seq,
                event_type: UpstreamTrigger::EVENT_TYPE,
                payload,
                occurred_at: jiff::Timestamp::now(),
            }],
        )
        .await
        .expect("append UpstreamTrigger");
}

async fn load_saga_events(store: &Arc<dyn EventStore>, saga_id: &SagaId) -> Vec<LoadedEvent> {
    store
        .load(LoadQuery::by_stream(saga_id))
        .await
        .expect("load saga stream")
        .try_collect()
        .await
        .expect("collect saga events")
}

fn count_outbox(events: &[LoadedEvent], pred: impl Fn(&OutboxKind) -> bool) -> usize {
    events
        .iter()
        .filter(|e| match outbox_kind_of(e) {
            Some(k) => pred(&k),
            None => false,
        })
        .count()
}

async fn wait_for_event_count(
    store: &Arc<dyn EventStore>,
    saga_id: &SagaId,
    expected_min: usize,
) -> Vec<LoadedEvent> {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let events = load_saga_events(store, saga_id).await;
        if events.len() >= expected_min || std::time::Instant::now() >= deadline {
            return events;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

#[allow(clippy::type_complexity)]
async fn spawn_two_tells_then_end_saga(
    system: &EventSourceSystem<JsonCodec>,
    saga_label: &str,
) -> (
    Arc<dyn EventStore>,
    SagaId,
    Arc<dyn EventStore>,
    AggregateId,
    Arc<AtomicUsize>,
    nitinol_saga::SagaProxy<TwoTellsThenEndSaga>,
) {
    let upstream_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let agg_id = AggregateId::new(format!("{saga_label}-agg"));

    let target_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let target_proxy = system
        .spawn_aggregate::<TargetAgg>(agg_id.clone(), Arc::clone(&target_store))
        .await;

    let saga_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let saga_id = SagaId::new(TWO_TELLS_THEN_END_SAGA_ID);
    let handle_count = Arc::new(AtomicUsize::new(0));

    let handle_count_clone = Arc::clone(&handle_count);
    let target_proxy_clone = target_proxy.clone();

    let saga_proxy = SagaProps::<TwoTellsThenEndSaga>::new(
        saga_id.clone(),
        Arc::clone(&saga_store),
        move || TwoTellsThenEndSaga {
            target: target_proxy_clone.clone(),
            handle_count: Arc::clone(&handle_count_clone),
        },
    )
    .with_codec(system.codec::<SagaMarker>())
    .with_subscription(
        Arc::clone(&upstream_store),
        system.codec::<UpstreamTrigger>(),
        SequenceCursor::Stream {
            key: agg_id.as_str().to_owned(),
            after: 0,
        },
    )
    .spawn(system.process_system())
    .await;

    (
        saga_store,
        saga_id,
        upstream_store,
        agg_id,
        handle_count,
        saga_proxy,
    )
}

#[tokio::test]
async fn multi_tell_then_end_settles_all_terminals_at_gapfree_sequences() {
    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::builder(ps)
        .with_codec::<JsonCodec>()
        .build();

    let (saga_store, saga_id, upstream_store, agg_id, _handle_count, _saga_proxy) =
        spawn_two_tells_then_end_saga(&system, "multi-tell-gapfree").await;

    append_upstream(&upstream_store, &agg_id, 1, "order-1").await;

    // 1 user event + 2 TellRequested + 1 Ended (from End) + 2 TellAcked = 6.
    let events = wait_for_event_count(&saga_store, &saga_id, 6).await;

    let requested = count_outbox(&events, |k| matches!(k, OutboxKind::TellRequested(_)));
    let acked = count_outbox(&events, |k| matches!(k, OutboxKind::TellAcked(_)));
    let failed = count_outbox(&events, |k| matches!(k, OutboxKind::TellFailed(_)));
    let ended = count_outbox(&events, |k| matches!(k, OutboxKind::Ended(_)));
    let user_events = events
        .iter()
        .filter(|e| e.event_type == SagaMarker::EVENT_TYPE)
        .count();

    assert_eq!(
        user_events,
        1,
        "the Persist branch must append exactly one user event; events: {:?}",
        events
            .iter()
            .map(|e| e.event_type.to_string())
            .collect::<Vec<_>>()
    );
    assert_eq!(
        requested, 2,
        "two tells must append exactly two TellRequested markers"
    );
    assert_eq!(
        acked,
        2,
        "deferred-stop must wait for BOTH executors: both terminal markers must \
         land (a premature stop after the first would drop the second TellAcked); \
         events: {:?}",
        events
            .iter()
            .map(|e| e.event_type.to_string())
            .collect::<Vec<_>>()
    );
    assert_eq!(
        failed, 0,
        "every dispatch succeeds first try, so no TellFailed must be emitted"
    );
    assert_eq!(
        ended, 1,
        "reaching End must persist exactly one durable Ended marker"
    );

    let mut seqs: Vec<u64> = events.iter().map(|e| e.sequence).collect();
    seqs.sort_unstable();
    assert_eq!(
        seqs,
        vec![1, 2, 3, 4, 5, 6],
        "saga sequences must be gap-free and consecutive across the atomic batch, \
         the Ended marker, and both terminal claims; a gap means a terminal claim \
         advanced the cursor without a durable append (or vice versa)"
    );
}

#[tokio::test]
async fn multi_tell_then_end_stops_saga_after_last_executor_settles() {
    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::builder(ps)
        .with_codec::<JsonCodec>()
        .build();

    let (saga_store, saga_id, upstream_store, agg_id, handle_count, _saga_proxy) =
        spawn_two_tells_then_end_saga(&system, "multi-tell-stops").await;

    append_upstream(&upstream_store, &agg_id, 1, "order-1").await;

    // 1 user event + 2 TellRequested + 1 Ended (from End) + 2 TellAcked = 6.
    let events = wait_for_event_count(&saga_store, &saga_id, 6).await;
    assert_eq!(
        count_outbox(&events, |k| matches!(k, OutboxKind::TellAcked(_))),
        2,
        "both terminal markers must settle before asserting the stop; events: {:?}",
        events
            .iter()
            .map(|e| e.event_type.to_string())
            .collect::<Vec<_>>()
    );

    tokio::time::sleep(Duration::from_millis(300)).await;

    append_upstream(&upstream_store, &agg_id, 2, "order-2").await;
    tokio::time::sleep(Duration::from_millis(500)).await;

    assert_eq!(
        handle_count.load(Ordering::SeqCst),
        1,
        "the saga must have stopped after End once the last executor settled; \
         Saga::handle must not run for the second upstream event"
    );

    let final_events = load_saga_events(&saga_store, &saga_id).await;
    let outbox = final_events
        .iter()
        .filter(|e| outbox_kind_of(e).is_some())
        .count();
    assert_eq!(
        outbox, 5,
        "exactly five outbox markers (2 TellRequested + 1 Ended + 2 TellAcked) must exist; \
         a stopped saga must not emit more for the second upstream event"
    );
}
