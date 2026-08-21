//! Correlation is evaluated **before** the draining guard.  (Contract `CT-03`,
//! ordering clause; interacts with `CT-08`.)
//!
//! `Receive<UpstreamMessage<_>>::recv` makes two decisions for a decoded event:
//! "is this event mine?" (correlation) and "am I still accepting events?"
//! (`Lifecycle::Draining`).  The order matters, and only one order is correct:
//!
//! - correlate → draining: an event belonging to *another* instance is declined
//!   silently even while this instance drains.
//! - draining → correlate: every event that merely *arrives* during draining is
//!   recorded as an `ended_saga_received_message` dead letter, including events
//!   that were never this saga's to begin with.  On a shared upstream that
//!   turns each terminating saga into a dead-letter generator.
//!
//! The stream below is ordered `MINE-1`, `SKIP-1`, `MINE-2`:
//!
//! | event    | correlate         | lifecycle at arrival | correct outcome              |
//! |----------|-------------------|----------------------|------------------------------|
//! | `MINE-1` | this instance     | `Running`            | `handle` → persist+tell+end  |
//! | `SKIP-1` | nobody (`None`)   | `Draining`           | ignored, silently            |
//! | `MINE-2` | this instance     | `Draining`           | `ended_saga_received_message`|
//!
//! So the *only* `ended_saga_received_message` dead letter must carry `MINE-2`'s
//! raw payload.  Under the reversed order `SKIP-1` is dead-lettered first, and
//! the payload assertion below fails on it — the poller delivers sequentially,
//! so `SKIP-1` is always evaluated before `MINE-2`.
//!
//! `BlockingTellTarget` keeps the saga in `Draining`: without it the outbox
//! executor can settle the tell before the poller delivers the remaining
//! events, and the saga stops before the ordering is ever exercised.  (Same
//! device as `saga_dead_letter_enqueue.rs`'s `BlockingDrainTellTarget`.)

#[path = "common/helpers.rs"]
mod common;
use common::JsonCodec;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use bytes::Bytes;
use futures_core::future::BoxFuture;
use futures_util::TryStreamExt;
use serde::{Deserialize, Serialize};
use tokio::sync::Notify;

use nitinol_eventsource::{
    system::EventSourceSystem, Aggregate, AggregateTellTarget, Context, Decider, Effect, Event,
    SequenceCursor, SystemEvent, TellError,
};
use nitinol_persistence::store::{EventStore, InMemoryEventStore};
use nitinol_persistence::{
    AppendingEvent, EventType, Family, LoadQuery, LoadedEvent, TypeName, Variant,
};
use nitinol_runtime::ProcessSystem;
use nitinol_saga::{
    DeadLetterEvent, Saga, SagaContext, SagaEffect, SagaFailure, SagaId, SagaProps, TellIntent,
};

const OWN_SAGA_ID: &str = "correlate-drain-order-saga";

const DEAD_LETTER_TYPE: EventType =
    EventType::new(Family::new("nitinol.saga"), TypeName::new("dead_letter"));

// Domain types

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Ping {
    tag: String,
}

impl Event for Ping {
    const EVENT_TYPE: EventType = EventType::new(
        Family::new("saga.correlate.drain_order"),
        TypeName::new("Ping"),
    );
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct SagaLog {
    note: String,
}

impl Event for SagaLog {
    const EVENT_TYPE: EventType = EventType::new(
        Family::new("saga.correlate.drain_order"),
        TypeName::new("SagaLog"),
    );
}

// Tell target that never settles until the test releases it, holding the saga
// in `Lifecycle::Draining` for the whole observation window.

#[derive(Default)]
struct DrainTargetAgg;

impl Aggregate for DrainTargetAgg {
    type Event = SagaLog;
    fn apply(&mut self, _event: SagaLog) {}
}

#[derive(Clone, Serialize, Deserialize)]
struct DrainCmd;

#[async_trait]
impl Decider<DrainCmd> for DrainTargetAgg {
    type Rejection = std::convert::Infallible;

    async fn decide(
        &self,
        _cmd: DrainCmd,
        _ctx: &mut Context,
    ) -> Result<Effect<SagaLog>, Self::Rejection> {
        Ok(Effect::empty())
    }
}

#[derive(Clone)]
struct BlockingTellTarget {
    notify: Arc<Notify>,
}

impl AggregateTellTarget<DrainTargetAgg> for BlockingTellTarget {
    fn tell<'a, C>(&'a self, _cmd: C) -> BoxFuture<'a, Result<(), TellError>>
    where
        DrainTargetAgg: Decider<C>,
        C: Send + Sync + 'static,
    {
        let notify = Arc::clone(&self.notify);
        Box::pin(async move {
            notify.notified().await;
            Ok(())
        })
    }

    fn aggregate_id_str(&self) -> &str {
        "correlate-drain-order-target"
    }
}

// Saga under test

struct EndOnFirstCorrelatedSaga {
    target: BlockingTellTarget,
    handle_count: Arc<AtomicUsize>,
}

#[async_trait]
impl Saga for EndOnFirstCorrelatedSaga {
    type SubscribedEvent = Ping;
    type Event = SagaLog;
    type ScheduledMessage = ();
    type Error = std::convert::Infallible;

    fn correlate(event: &Self::SubscribedEvent) -> Option<SagaId> {
        event
            .tag
            .starts_with("MINE-")
            .then(|| SagaId::new(OWN_SAGA_ID))
    }

    fn apply(&mut self, _event: Self::Event) {}

    async fn handle(
        &mut self,
        event: Self::SubscribedEvent,
        _ctx: &mut SagaContext,
    ) -> Result<SagaEffect<Self::Event>, Self::Error> {
        self.handle_count.fetch_add(1, Ordering::SeqCst);
        // persist + one in-flight tell + end → the saga enters Draining rather
        // than stopping outright, and stays there until the tell settles.
        let intent = TellIntent::new(self.target.clone(), DrainCmd);
        Ok(SagaEffect::persist(SagaLog { note: event.tag })
            .combine(SagaEffect::tell_intent(intent))
            .then_end())
    }
}

// Helpers

fn encode_ping(tag: &str) -> Bytes {
    serde_json::to_vec(&Ping {
        tag: tag.to_owned(),
    })
    .map(Bytes::from)
    .expect("encode Ping must succeed")
}

async fn append_ping(store: &Arc<dyn EventStore>, stream_key: &str, sequence: u64, tag: &str) {
    store
        .append(
            stream_key,
            vec![AppendingEvent {
                sequence,
                event_type: Ping::EVENT_TYPE,
                payload: encode_ping(tag),
                occurred_at: jiff::Timestamp::now(),
            }],
        )
        .await
        .expect("append Ping must succeed");
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

fn ended_saga_dead_letters(events: &[LoadedEvent]) -> Vec<&LoadedEvent> {
    events
        .iter()
        .filter(|e| {
            e.event_type.type_key() == DEAD_LETTER_TYPE.type_key()
                && e.event_type.variant() == Some(Variant::new("ended_saga_received_message"))
        })
        .collect()
}

async fn wait_for_ended_saga_dead_letter(
    store: &Arc<dyn EventStore>,
    saga_id: &SagaId,
) -> Vec<LoadedEvent> {
    let deadline = Instant::now() + Duration::from_secs(6);
    loop {
        let events = load_saga_events(store, saga_id).await;
        if !ended_saga_dead_letters(&events).is_empty() {
            return events;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for an `ended_saga_received_message` dead letter; \
             the saga must stay in Draining long enough to receive the events \
             that follow its End"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

// Test

/// Given a saga that has ended and is draining,
/// when an event that correlates to nobody arrives ahead of one that correlates
/// to this saga,
/// then only the correlated event becomes an `ended_saga_received_message` dead
/// letter — correlation is decided first, so the draining guard never sees an
/// event that was not this saga's.
#[tokio::test]
async fn draining_saga_dead_letters_only_events_it_correlates_to() {
    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::builder(ps)
        .with_codec::<JsonCodec>()
        .build();

    // Pre-load all three events: the direct poller reads the batch in one
    // `store.load()` and delivers them sequentially, so SKIP-1 is always
    // evaluated before MINE-2.
    let upstream_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let upstream_key = "correlate-drain-order-upstream";
    append_ping(&upstream_store, upstream_key, 1, "MINE-1").await;
    append_ping(&upstream_store, upstream_key, 2, "SKIP-1").await;
    append_ping(&upstream_store, upstream_key, 3, "MINE-2").await;

    let saga_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let saga_id = SagaId::new(OWN_SAGA_ID);
    let handle_count = Arc::new(AtomicUsize::new(0));
    let drain_unblock = Arc::new(Notify::new());

    let handle_count_for_saga = Arc::clone(&handle_count);
    let drain_unblock_for_saga = Arc::clone(&drain_unblock);

    let _proxy = SagaProps::<EndOnFirstCorrelatedSaga>::new(
        saga_id.clone(),
        Arc::clone(&saga_store),
        move || EndOnFirstCorrelatedSaga {
            target: BlockingTellTarget {
                notify: Arc::clone(&drain_unblock_for_saga),
            },
            handle_count: Arc::clone(&handle_count_for_saga),
        },
    )
    .with_codec(system.codec::<SagaLog>())
    .with_subscription(
        Arc::clone(&upstream_store),
        system.codec::<Ping>(),
        SequenceCursor::Stream {
            key: upstream_key.to_owned(),
            after: 0,
        },
    )
    .spawn(system.process_system())
    .await;

    let events = wait_for_ended_saga_dead_letter(&saga_store, &saga_id).await;

    // Dead letter observed — release the outbox executor so the saga drains.
    drain_unblock.notify_one();

    let recorded = ended_saga_dead_letters(&events)
        .into_iter()
        .map(|e| {
            match DeadLetterEvent::decode(&e.payload)
                .expect("ended_saga_received_message dead letter must decode")
                .failure
            {
                SagaFailure::EndedSagaReceivedMessage { message } => message,
                other => panic!("expected EndedSagaReceivedMessage, got {other:?}"),
            }
        })
        .collect::<Vec<_>>();

    assert_eq!(
        recorded,
        vec![encode_ping("MINE-2")],
        "only the event this saga correlates to may be dead-lettered by the \
         draining guard; SKIP-1 correlates to nobody and must have been \
         declined before the lifecycle was ever consulted"
    );

    assert_eq!(
        handle_count.load(Ordering::SeqCst),
        1,
        "`handle` must run for MINE-1 only: SKIP-1 is declined by correlation \
         and MINE-2 is dropped by the draining guard"
    );
}
