//! Issue #51 (Saga DLQ) — `EnqueuePolicy` filtering (G-26).
//!
//! The default policy enqueues **every** failure kind.  A user may override the
//! policy via `SagaProps::with_enqueue_policy(...)` to suppress selected (or
//! all) failures; a suppressed failure must leave no dead letter on the saga
//! stream.  Both directions are asserted here against the public wire contract
//! (`nitinol.saga.dead_letter`), using a `handle` failure as the trigger.
//!
//! Contract assumed (order.md G-26):
//! ```ignore
//! trait EnqueuePolicy { fn decide(&self, failure: &SagaFailure) -> EnqueueDecision; }
//! enum EnqueueDecision { Enqueue, Ignore }
//! ```
//! and the builder entry `SagaProps::with_enqueue_policy(Arc<dyn EnqueuePolicy>)`.
//! The custom policy below references only the trait method signature and the
//! `EnqueueDecision::Ignore` variant — never a `SagaFailure` field — so it does
//! not constrain the failure enum's internal shape.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use bytes::Bytes;
use futures_util::TryStreamExt;
use serde::{Deserialize, Serialize};
use tokio::sync::Notify;

use nitinol_eventsource::codec::Codec;
use nitinol_eventsource::{system::EventSourceSystem, Event, SequenceCursor};
use nitinol_persistence::store::{EventStore, InMemoryEventStore};
use nitinol_persistence::{
    AppendingEvent, EventType, Family, LoadQuery, LoadedEvent, TypeName, Variant,
};
use nitinol_runtime::ProcessSystem;
use nitinol_saga::{
    EnqueueDecision, EnqueuePolicy, Saga, SagaContext, SagaEffect, SagaFailure, SagaId, SagaProps,
};

// ---------------------------------------------------------------------------
// Local codec + domain
// ---------------------------------------------------------------------------

#[derive(Default)]
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

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Ping;

impl Event for Ping {
    const EVENT_TYPE: EventType = EventType::new(Family::new("dlq_policy"), TypeName::new("Ping"));
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct SagaLog;

impl Event for SagaLog {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("dlq_policy"), TypeName::new("SagaLog"));
}

#[derive(Debug)]
struct SagaBoom;

impl std::fmt::Display for SagaBoom {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "handle boom")
    }
}

impl std::error::Error for SagaBoom {}

/// A saga whose `handle` always fails, counting invocations so a test can prove
/// the failure actually occurred even when no dead letter is enqueued.
struct AlwaysFailSaga {
    handled: Arc<Notify>,
    handle_count: Arc<AtomicUsize>,
}

#[async_trait]
impl Saga for AlwaysFailSaga {
    type SubscribedEvent = Ping;
    type Event = SagaLog;
    type State = ();
    type Error = SagaBoom;
    type ScheduledMessage = ();

    fn apply(&mut self, _event: SagaLog) {}

    async fn handle(
        &mut self,
        _event: Ping,
        _ctx: &mut SagaContext,
    ) -> Result<SagaEffect<SagaLog>, Self::Error> {
        self.handle_count.fetch_add(1, Ordering::SeqCst);
        self.handled.notify_one();
        Err(SagaBoom)
    }
}

/// Suppress every failure — references only the trait signature and the
/// `Ignore` variant, so it stays agnostic to `SagaFailure`'s internals.
struct IgnoreAll;

impl EnqueuePolicy for IgnoreAll {
    fn decide(&self, _failure: &SagaFailure) -> EnqueueDecision {
        EnqueueDecision::Ignore
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

const DEAD_LETTER_TYPE: EventType =
    EventType::new(Family::new("nitinol.saga"), TypeName::new("dead_letter"));

fn count_dead_letters(events: &[LoadedEvent]) -> usize {
    events
        .iter()
        .filter(|e| e.event_type.type_key() == DEAD_LETTER_TYPE.type_key())
        .count()
}

fn count_dead_letter_variant(events: &[LoadedEvent], variant: &'static str) -> usize {
    events
        .iter()
        .filter(|e| {
            e.event_type.type_key() == DEAD_LETTER_TYPE.type_key()
                && e.event_type.variant() == Some(Variant::new(variant))
        })
        .count()
}

async fn append_ping(store: &Arc<dyn EventStore>, stream_key: &str, sequence: u64) {
    let payload = serde_json::to_vec(&Ping)
        .map(Bytes::from)
        .expect("encode Ping must succeed");
    store
        .append(
            stream_key,
            vec![AppendingEvent {
                sequence,
                event_type: Ping::EVENT_TYPE,
                payload,
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

/// Poll the saga stream until at least one `handle_failed` dead letter is
/// present, or the timeout elapses.
async fn wait_for_handle_failed(
    store: &Arc<dyn EventStore>,
    saga_id: &SagaId,
    timeout: Duration,
) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        let events = load_saga_events(store, saga_id).await;
        if count_dead_letter_variant(&events, "handle_failed") >= 1 {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

// ---------------------------------------------------------------------------
// Test A — default policy enqueues
// ---------------------------------------------------------------------------

/// Given no explicit enqueue policy (the default is "enqueue all"),
/// When a handle failure occurs,
/// Then a `handle_failed` dead letter is enqueued.
#[tokio::test]
async fn default_policy_enqueues_the_failure() {
    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::new(ps).with_codec::<JsonCodec>().build();

    let upstream_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    append_ping(&upstream_store, "dlq-default-upstream", 1).await;

    let saga_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let saga_id = SagaId::new("dlq-default-saga");
    let handled = Arc::new(Notify::new());
    let handle_count = Arc::new(AtomicUsize::new(0));

    let handled_for_saga = Arc::clone(&handled);
    let handle_count_for_saga = Arc::clone(&handle_count);
    let routed = saga_id.clone();
    let _proxy = SagaProps::<AlwaysFailSaga>::new(saga_id.clone(), Arc::clone(&saga_store), move || {
        AlwaysFailSaga {
            handled: Arc::clone(&handled_for_saga),
            handle_count: Arc::clone(&handle_count_for_saga),
        }
    })
    .with_codec(system.codec::<SagaLog>())
    .with_subscription(
        Arc::clone(&upstream_store),
        system.codec::<Ping>(),
        SequenceCursor::Stream {
            key: "dlq-default-upstream".to_owned(),
            after: 0,
        },
        move |_event: &Ping| Some(routed.clone()),
    )
    .spawn(system.process_system())
    .await;

    tokio::time::timeout(Duration::from_secs(3), handled.notified())
        .await
        .expect("Saga::handle must run within 3 seconds");

    let found = wait_for_handle_failed(&saga_store, &saga_id, Duration::from_secs(5)).await;

    let events = load_saga_events(&saga_store, &saga_id).await;
    assert!(
        found,
        "the default policy must enqueue the failure; dead letters on stream: {}",
        count_dead_letters(&events)
    );
    assert_eq!(
        count_dead_letter_variant(&events, "handle_failed"),
        1,
        "default policy must enqueue exactly one `handle_failed` dead letter"
    );
}

// ---------------------------------------------------------------------------
// Test B — custom "ignore all" policy suppresses enqueue
// ---------------------------------------------------------------------------

/// Given an `IgnoreAll` enqueue policy,
/// When a handle failure occurs,
/// Then no dead letter is written to the saga stream — even though `handle`
/// was invoked and failed.
#[tokio::test]
async fn ignore_all_policy_suppresses_enqueue() {
    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::new(ps).with_codec::<JsonCodec>().build();

    let upstream_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    append_ping(&upstream_store, "dlq-ignore-upstream", 1).await;

    let saga_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let saga_id = SagaId::new("dlq-ignore-saga");
    let handled = Arc::new(Notify::new());
    let handle_count = Arc::new(AtomicUsize::new(0));

    let handled_for_saga = Arc::clone(&handled);
    let handle_count_for_saga = Arc::clone(&handle_count);
    let routed = saga_id.clone();
    let _proxy = SagaProps::<AlwaysFailSaga>::new(saga_id.clone(), Arc::clone(&saga_store), move || {
        AlwaysFailSaga {
            handled: Arc::clone(&handled_for_saga),
            handle_count: Arc::clone(&handle_count_for_saga),
        }
    })
    .with_codec(system.codec::<SagaLog>())
    .with_enqueue_policy(Arc::new(IgnoreAll))
    .with_subscription(
        Arc::clone(&upstream_store),
        system.codec::<Ping>(),
        SequenceCursor::Stream {
            key: "dlq-ignore-upstream".to_owned(),
            after: 0,
        },
        move |_event: &Ping| Some(routed.clone()),
    )
    .spawn(system.process_system())
    .await;

    tokio::time::timeout(Duration::from_secs(3), handled.notified())
        .await
        .expect("Saga::handle must run within 3 seconds");

    // The failure has occurred (handle notified).  Give the DLQ append path
    // ample opportunity to run, then assert it did not.
    tokio::time::sleep(Duration::from_millis(500)).await;

    assert!(
        handle_count.load(Ordering::SeqCst) >= 1,
        "the handle failure must actually have occurred"
    );

    let events = load_saga_events(&saga_store, &saga_id).await;
    assert_eq!(
        count_dead_letters(&events),
        0,
        "an `Ignore` decision must suppress the dead letter entirely; \
         stream held: {:?}",
        events
            .iter()
            .map(|e| e.event_type.to_string())
            .collect::<Vec<_>>()
    );
}
