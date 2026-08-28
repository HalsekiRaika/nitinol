//! The degenerate single-instance manager reproduces the DLQ and crash-restart
//! settings of `SagaProps`.
//!
//! `saga_manager_degenerate_scheduler.rs` covers the schedule-effect half of the
//! migration path.  This file covers the rest of the instance settings the
//! standalone builder accepts — `with_dead_letter_subscriber`,
//! `with_enqueue_policy` and `with_crash_restart_factory` — each of which is a
//! separate value that has to travel from the manager builder into the spawned
//! instance.  A manager that accepted them and dropped them on the way would
//! leave the dead letters its instances write with no subscriber, ignore the
//! configured filter, and turn a passivated in-flight tell into a permanent
//! failure.  The subscriber has to reach every instance the manager spawns,
//! including one that replays as already ended: that instance stays resident
//! under the manager and keeps dead-lettering each arrival.

#[path = "common/helpers.rs"]
mod common;
use common::{
    encode_outbox_ended, encode_outbox_tell_requested, outbox_kind_of, JsonCodec, OutboxKind,
    OUTBOX_MARKER,
};

use std::convert::Infallible;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use bytes::Bytes;
use futures_util::TryStreamExt;
use serde::{Deserialize, Serialize};
use tokio::sync::Notify;

use nitinol_eventsource::test_helpers::MockAggregateProxy;
use nitinol_eventsource::{
    system::EventSourceSystem, Aggregate, Decider, Decision, Event, SequenceCursor,
};
use nitinol_persistence::store::{EventStore, InMemoryEventStore};
use nitinol_persistence::{
    AppendingEvent, EventType, Family, LoadQuery, LoadedEvent, TypeName, Variant,
};
use nitinol_runtime::process::{Process, ProcessContext, Receive};
use nitinol_runtime::{ProcessSystem, Props};
use nitinol_saga::{
    DeadLetterEvent, EnqueueDecision, EnqueuePolicy, Saga, SagaContext, SagaEffect, SagaFailure,
    SagaId, SagaManagerProps, TellIntent,
};

/// The single instance every upstream event correlates to — the degenerate case
/// the migration path is about.
const RESIDENT_SAGA_ID: &str = "mgr-settings-degenerate-1";

// Upstream and saga domain

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Ping;

impl Event for Ping {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("mgr.settings"), TypeName::new("Ping"));
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct SagaLog;

impl Event for SagaLog {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("mgr.settings"), TypeName::new("SagaLog"));
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
    type Error = SagaBoom;
    type ScheduledMessage = ();

    fn correlate(_event: &Self::SubscribedEvent) -> Option<SagaId> {
        Some(SagaId::new(RESIDENT_SAGA_ID))
    }

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

// The DLQ subscriber process under test.

struct DlqCollector {
    received: Arc<Mutex<Vec<DeadLetterEvent>>>,
}

impl Process for DlqCollector {}

impl Receive<DeadLetterEvent> for DlqCollector {
    type Response = ();
    type Error = Infallible;

    async fn recv(
        &mut self,
        msg: DeadLetterEvent,
        _ctx: &mut ProcessContext<Self>,
    ) -> Result<(), Infallible> {
        self.received
            .lock()
            .expect("received mutex is never poisoned: no holder panics while the guard is alive")
            .push(msg);
        Ok(())
    }
}

// The aggregate a re-dispatched tell is addressed to.

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Reserved;

impl Event for Reserved {
    const EVENT_TYPE: EventType =
        EventType::new(Family::new("mgr.settings"), TypeName::new("Reserved"));
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Reserve;

#[derive(Clone, Debug, Default)]
struct Inventory;

impl Aggregate for Inventory {
    type Event = Reserved;

    fn apply(&mut self, _event: Self::Event) {}
}

impl Decider<Reserve> for Inventory {
    type Output = ();
    type Rejection = Infallible;

    fn decide(&self, _command: Reserve) -> Decision<Reserved, (), Self::Rejection> {
        Decision::persist(vec![Reserved]).output(())
    }
}

// Helpers

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

/// Seed a durable `Ended` outbox marker so the instance replays as terminated.
async fn seed_ended_marker(store: &Arc<dyn EventStore>, saga_id: &SagaId, sequence: u64) {
    store
        .append(
            saga_id.as_str(),
            vec![AppendingEvent {
                sequence,
                event_type: OUTBOX_MARKER,
                payload: encode_outbox_ended(),
                occurred_at: jiff::Timestamp::now(),
            }],
        )
        .await
        .expect("seed Ended marker must succeed");
}

async fn load_stream(store: &Arc<dyn EventStore>, saga_id: &SagaId) -> Vec<LoadedEvent> {
    store
        .load(LoadQuery::by_stream(saga_id))
        .await
        .expect("load saga stream must succeed")
        .try_collect()
        .await
        .expect("collect saga events must succeed")
}

// Test A — the DLQ subscriber reaches the manager-spawned instance

/// Given a manager configured with `SagaManagerProps::with_dead_letter_subscriber`
/// whose `Saga::correlate` always answers the same id,
/// when the instance it spawns enqueues a dead letter,
/// then the registered subscriber receives that instance's `DeadLetterEvent` —
/// the same outcome `SagaProps::with_dead_letter_subscriber` gives the
/// standalone builder.  Without carrying the subscriber into the spawned
/// instance the dead letter is still written to the instance's stream but no
/// subscriber ever sees it.
#[tokio::test]
async fn manager_with_dead_letter_subscriber_delivers_the_instances_dead_letter() {
    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::builder(ps)
        .with_codec::<JsonCodec>()
        .build();

    let received = Arc::new(Mutex::new(Vec::<DeadLetterEvent>::new()));
    let received_for_proc = Arc::clone(&received);
    let subscriber_proxy = system
        .process_system()
        .spawn(Props::new(move || DlqCollector {
            received: Arc::clone(&received_for_proc),
        }))
        .await;

    let upstream_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    append_ping(&upstream_store, "mgr-dlq-sub-upstream", 1).await;

    let saga_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let handled = Arc::new(Notify::new());
    let handle_count = Arc::new(AtomicUsize::new(0));
    let handled_for_saga = Arc::clone(&handled);
    let handle_count_for_saga = Arc::clone(&handle_count);

    let _manager_proxy =
        SagaManagerProps::<AlwaysFailSaga>::new(Arc::clone(&saga_store), move || AlwaysFailSaga {
            handled: Arc::clone(&handled_for_saga),
            handle_count: Arc::clone(&handle_count_for_saga),
        })
        .with_codec(system.codec::<SagaLog>())
        .with_dead_letter_subscriber(subscriber_proxy)
        .with_subscription(
            Arc::clone(&upstream_store),
            system.codec::<Ping>(),
            SequenceCursor::Stream {
                key: "mgr-dlq-sub-upstream".to_owned(),
                after: 0,
            },
        )
        .spawn(system.process_system())
        .await;

    let deadline = Instant::now() + Duration::from_secs(8);
    let delivered = loop {
        let seen = received
            .lock()
            .expect("received mutex is never poisoned: no holder panics while the guard is alive")
            .clone();
        if !seen.is_empty() {
            break seen;
        }
        assert!(
            Instant::now() < deadline,
            "the subscriber registered on the manager must receive the dead letter \
             its spawned instance enqueued; handle ran {} time(s)",
            handle_count.load(Ordering::SeqCst)
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    };

    assert!(
        delivered
            .iter()
            .all(|event| event.saga_id.as_str() == RESIDENT_SAGA_ID),
        "every delivered dead letter must be attributed to the manager-spawned \
         instance's own id; got {delivered:?}"
    );
}

// Test B — the enqueue policy configured on the manager filters the instance

/// Given a manager configured with an `IgnoreAll` enqueue policy,
/// when the instance it spawns fails to handle an upstream event,
/// then no dead letter is written to that instance's stream — the manager's
/// policy reaches the instance instead of the instance falling back to the
/// default "enqueue every kind".  This is the rejecting half of the pair Test A
/// covers on the accepting side.
#[tokio::test]
async fn manager_with_enqueue_policy_suppresses_the_instances_dead_letter() {
    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::builder(ps)
        .with_codec::<JsonCodec>()
        .build();

    let upstream_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    append_ping(&upstream_store, "mgr-dlq-policy-upstream", 1).await;

    let saga_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let saga_id = SagaId::new(RESIDENT_SAGA_ID);
    let handled = Arc::new(Notify::new());
    let handle_count = Arc::new(AtomicUsize::new(0));
    let handled_for_saga = Arc::clone(&handled);
    let handle_count_for_saga = Arc::clone(&handle_count);

    let _manager_proxy =
        SagaManagerProps::<AlwaysFailSaga>::new(Arc::clone(&saga_store), move || AlwaysFailSaga {
            handled: Arc::clone(&handled_for_saga),
            handle_count: Arc::clone(&handle_count_for_saga),
        })
        .with_codec(system.codec::<SagaLog>())
        .with_enqueue_policy(Arc::new(IgnoreAll))
        .with_subscription(
            Arc::clone(&upstream_store),
            system.codec::<Ping>(),
            SequenceCursor::Stream {
                key: "mgr-dlq-policy-upstream".to_owned(),
                after: 0,
            },
        )
        .spawn(system.process_system())
        .await;

    tokio::time::timeout(Duration::from_secs(5), handled.notified())
        .await
        .expect("the manager-spawned instance's Saga::handle must run within 5 seconds");

    // The failure has occurred (handle notified).  Give the DLQ append path
    // ample opportunity to run, then assert it did not.
    tokio::time::sleep(Duration::from_millis(500)).await;

    assert!(
        handle_count.load(Ordering::SeqCst) >= 1,
        "the handle failure must actually have occurred"
    );

    let events = load_stream(&saga_store, &saga_id).await;
    assert_eq!(
        count_dead_letters(&events),
        0,
        "the manager's `Ignore` policy must suppress the instance's dead letter \
         entirely; stream held: {:?}",
        events
            .iter()
            .map(|e| e.event_type.to_string())
            .collect::<Vec<_>>()
    );
    assert_eq!(
        count_dead_letter_variant(&events, "handle_failed"),
        0,
        "no `handle_failed` dead letter may survive an `Ignore` decision"
    );
}

// Test C — the crash-restart factory reaches the revived instance

/// A saga that never acts on upstream events: the tell under test is the one
/// already recorded on the instance's stream, re-dispatched by replay.
#[derive(Default)]
struct InertSaga;

#[async_trait]
impl Saga for InertSaga {
    type SubscribedEvent = Ping;
    type Event = SagaLog;
    type Error = Infallible;
    type ScheduledMessage = ();

    fn correlate(_event: &Self::SubscribedEvent) -> Option<SagaId> {
        Some(SagaId::new(RESIDENT_SAGA_ID))
    }

    fn apply(&mut self, _event: SagaLog) {}

    async fn handle(
        &mut self,
        _event: Ping,
        _ctx: &mut SagaContext,
    ) -> Result<SagaEffect<SagaLog>, Self::Error> {
        Ok(SagaEffect::None)
    }
}

/// Given an instance stream that carries a pending `TellRequested` with
/// crash-restart bytes (what a tell left in flight when the instance was
/// passivated looks like on disk), and a manager configured with
/// `SagaManagerProps::with_crash_restart_factory`,
/// when an upstream event makes the manager spawn that instance,
/// then replay must re-dispatch the pending tell through the factory and record
/// `TellAcked` — not the synthetic `TellFailed` an instance without a factory
/// records.  The in-memory intent is gone across a passivation, so the factory
/// is the only thing that can reconstruct the tell.
#[tokio::test]
async fn manager_with_crash_restart_factory_redispatches_the_instances_pending_tell() {
    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::builder(ps)
        .with_codec::<JsonCodec>()
        .build();

    let mock = MockAggregateProxy::<Inventory>::new();

    let saga_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let saga_id = SagaId::new(RESIDENT_SAGA_ID);

    // Seed the instance's own stream with an unsettled tell carrying
    // crash-restart bytes — the state a passivated instance leaves behind.
    saga_store
        .append(
            saga_id.as_str(),
            vec![AppendingEvent {
                sequence: 1,
                event_type: OUTBOX_MARKER,
                payload: encode_outbox_tell_requested(1, Some(b"reserve")),
                occurred_at: jiff::Timestamp::now(),
            }],
        )
        .await
        .expect("seed TellRequested must succeed");

    let upstream_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    append_ping(&upstream_store, "mgr-crash-restart-upstream", 1).await;

    let mock_for_factory = mock.clone();
    let _manager_proxy =
        SagaManagerProps::<InertSaga>::new(Arc::clone(&saga_store), InertSaga::default)
            .with_codec(system.codec::<SagaLog>())
            .with_crash_restart_factory(move |_bytes: &[u8]| -> Option<TellIntent> {
                Some(TellIntent::new::<Inventory, Reserve, _>(
                    mock_for_factory.clone(),
                    Reserve,
                ))
            })
            .with_subscription(
                Arc::clone(&upstream_store),
                system.codec::<Ping>(),
                SequenceCursor::Stream {
                    key: "mgr-crash-restart-upstream".to_owned(),
                    after: 0,
                },
            )
            .spawn(system.process_system())
            .await;

    let deadline = Instant::now() + Duration::from_secs(8);
    let events = loop {
        let events = load_stream(&saga_store, &saga_id).await;
        if events
            .iter()
            .any(|e| matches!(outbox_kind_of(e), Some(OutboxKind::TellAcked(_))))
        {
            break events;
        }
        assert!(
            Instant::now() < deadline,
            "the manager's crash-restart factory must re-dispatch the instance's \
             pending tell; stream held: {:?}",
            events
                .iter()
                .map(|e| e.event_type.to_string())
                .collect::<Vec<_>>()
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    };

    assert!(
        !events
            .iter()
            .any(|e| matches!(outbox_kind_of(e), Some(OutboxKind::TellFailed(_)))),
        "a re-dispatched tell must not also be recorded as failed; stream held: {:?}",
        events
            .iter()
            .map(|e| e.event_type.to_string())
            .collect::<Vec<_>>()
    );
}

// Test D — the DLQ subscriber also reaches an instance that replays as ended

/// Given a manager configured with `SagaManagerProps::with_dead_letter_subscriber`
/// and an instance whose own stream already carries a durable `Ended` marker,
/// when an upstream event correlates to that instance,
/// then the `SagaFailure::EndedSagaReceivedMessage` it records must reach the
/// registered subscriber.  Unlike the standalone builder's saga — which owns its
/// subscription and stops on replaying `Ended` — a manager-spawned instance
/// stays resident and keeps dead-lettering every arrival the shared subscription
/// routes to it, so skipping its DLQ poller would make those records permanently
/// unobservable: no pull API exists, the subscriber is the only way to see them.
#[tokio::test]
async fn manager_dead_letter_subscriber_receives_an_ended_instances_dead_letter() {
    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::builder(ps)
        .with_codec::<JsonCodec>()
        .build();

    let received = Arc::new(Mutex::new(Vec::<DeadLetterEvent>::new()));
    let received_for_proc = Arc::clone(&received);
    let subscriber_proxy = system
        .process_system()
        .spawn(Props::new(move || DlqCollector {
            received: Arc::clone(&received_for_proc),
        }))
        .await;

    let upstream_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    append_ping(&upstream_store, "mgr-dlq-ended-upstream", 1).await;

    let saga_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let saga_id = SagaId::new(RESIDENT_SAGA_ID);
    seed_ended_marker(&saga_store, &saga_id, 1).await;

    let _manager_proxy =
        SagaManagerProps::<InertSaga>::new(Arc::clone(&saga_store), InertSaga::default)
            .with_codec(system.codec::<SagaLog>())
            .with_dead_letter_subscriber(subscriber_proxy)
            .with_subscription(
                Arc::clone(&upstream_store),
                system.codec::<Ping>(),
                SequenceCursor::Stream {
                    key: "mgr-dlq-ended-upstream".to_owned(),
                    after: 0,
                },
            )
            .spawn(system.process_system())
            .await;

    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        let delivered = received
            .lock()
            .expect("received mutex is never poisoned: no holder panics while the guard is alive")
            .clone();
        if delivered.iter().any(|event| {
            event.saga_id.as_str() == RESIDENT_SAGA_ID
                && matches!(event.failure, SagaFailure::EndedSagaReceivedMessage { .. })
        }) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the subscriber registered on the manager must receive the dead letter \
             the ended instance recorded for the record routed to it; got {delivered:?}"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    let events = load_stream(&saga_store, &saga_id).await;
    assert_eq!(
        count_dead_letter_variant(&events, "ended_saga_received_message"),
        1,
        "exactly one dead letter must be recorded for the single record the ended \
         instance received; stream held: {:?}",
        events
            .iter()
            .map(|e| e.event_type.to_string())
            .collect::<Vec<_>>()
    );
}
