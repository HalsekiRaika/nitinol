//! Saga DLQ subscriber delivery via `DurableStream`.
//!
//! `SagaProps::with_dead_letter_subscriber(proxy)` registers a subscriber
//! process `P: Receive<DeadLetterEvent>`.  When the saga enqueues a dead letter
//! (here via a `handle` failure), the subscriber must receive the
//! `DeadLetterEvent` — delivered over a DurableStream so it survives subscriber
//! downtime by catching up from the saga's EventStore.
//!
//! Contract assumed:
//! ```ignore
//! let saga = SagaProps::new(..)
//!     .with_dead_letter_subscriber(compensation_handler_proxy)  // ProcessProxy<P>
//!     .spawn(system).await;
//! // where P: Receive<DeadLetterEvent>
//! ```
//!
//! The collector records only the *count* of received dead letters, so this
//! test does not couple to the `DeadLetterEvent` field layout — it verifies
//! delivery, which is the requirement.

use std::convert::Infallible;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use bytes::Bytes;
use serde::{Deserialize, Serialize};

use nitinol_eventsource::codec::Codec;
use nitinol_eventsource::{
    appending_system_event, system::EventSourceSystem, Event, SequenceCursor,
};
use nitinol_persistence::store::{EventStore, InMemoryEventStore};
use nitinol_persistence::{AppendingEvent, EventType, Family, TypeName};
use nitinol_runtime::process::{Process, ProcessContext, Receive};
use nitinol_runtime::{ProcessSystem, Props};
use nitinol_saga::{
    DeadLetterEvent, Saga, SagaContext, SagaEffect, SagaFailure, SagaId, SagaProps, SourceContext,
};

// Local codec + domain

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
    const EVENT_TYPE: EventType = EventType::new(Family::new("dlq_sub"), TypeName::new("Ping"));
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct SagaLog;

impl Event for SagaLog {
    const EVENT_TYPE: EventType = EventType::new(Family::new("dlq_sub"), TypeName::new("SagaLog"));
}

#[derive(Debug)]
struct SagaBoom;

impl std::fmt::Display for SagaBoom {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "handle boom")
    }
}

impl std::error::Error for SagaBoom {}

struct FailSaga;

#[async_trait]
impl Saga for FailSaga {
    type SubscribedEvent = Ping;
    type Event = SagaLog;
    type Error = SagaBoom;
    type ScheduledMessage = ();

    fn apply(&mut self, _event: SagaLog) {}

    async fn handle(
        &mut self,
        _event: Ping,
        _ctx: &mut SagaContext,
    ) -> Result<SagaEffect<SagaLog>, Self::Error> {
        Err(SagaBoom)
    }
}

// The DLQ subscriber process under test.

struct DlqCollector {
    received: Arc<AtomicUsize>,
}

impl Process for DlqCollector {}

impl Receive<DeadLetterEvent> for DlqCollector {
    type Response = ();
    type Error = Infallible;

    async fn recv(
        &mut self,
        _msg: DeadLetterEvent,
        _ctx: &mut ProcessContext<Self>,
    ) -> Result<(), Infallible> {
        self.received.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

// Helper

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

// Test

/// Given a saga configured with a dead-letter subscriber,
/// When the saga enqueues a dead letter (a handle failure here),
/// Then the subscriber process receives the corresponding `DeadLetterEvent`.
#[tokio::test]
async fn dead_letter_subscriber_receives_the_enqueued_event() {
    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::new(ps).with_codec::<JsonCodec>().build();

    // Spawn the subscriber process and obtain its ProcessProxy<DlqCollector>.
    let received = Arc::new(AtomicUsize::new(0));
    let received_for_proc = Arc::clone(&received);
    let subscriber_proxy = system
        .process_system()
        .spawn(Props::new(move || DlqCollector {
            received: Arc::clone(&received_for_proc),
        }))
        .await;

    let upstream_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    append_ping(&upstream_store, "dlq-sub-upstream", 1).await;

    let saga_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let saga_id = SagaId::new("dlq-sub-saga");

    let routed = saga_id.clone();
    let _saga_proxy =
        SagaProps::<FailSaga>::new(saga_id.clone(), Arc::clone(&saga_store), || FailSaga)
            .with_codec(system.codec::<SagaLog>())
            .with_dead_letter_subscriber(subscriber_proxy)
            .with_subscription(
                Arc::clone(&upstream_store),
                system.codec::<Ping>(),
                SequenceCursor::Stream {
                    key: "dlq-sub-upstream".to_owned(),
                    after: 0,
                },
                move |_event: &Ping| Some(routed.clone()),
            )
            .spawn(system.process_system())
            .await;

    let deadline = Instant::now() + Duration::from_secs(6);
    loop {
        if received.load(Ordering::SeqCst) >= 1 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the dead-letter subscriber must receive the enqueued DeadLetterEvent within the deadline"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    assert!(
        received.load(Ordering::SeqCst) >= 1,
        "the subscriber must have received at least one dead letter"
    );
}

/// Given a saga stream that already contains a `DeadLetterEvent` (simulating a
/// subscriber that was down when the dead letter was enqueued),
/// When the saga is spawned with `with_dead_letter_subscriber(subscriber_proxy)`,
/// Then the subscriber catches up from the EventStore and receives the
/// pre-existing dead letter — the subscriber can catch up from the EventStore
/// even if it was down when the dead letter landed.
#[tokio::test]
async fn dead_letter_subscriber_catchup_delivers_pre_existing_dead_letters() {
    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::new(ps).with_codec::<JsonCodec>().build();

    let saga_id = SagaId::new("dlq-sub-catchup-saga");
    let saga_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());

    // Pre-append a DeadLetterEvent at sequence 1 — simulating a dead letter that
    // was written in a previous saga incarnation while the subscriber was down.
    let pre_existing = DeadLetterEvent {
        seq: 1,
        saga_id: saga_id.clone(),
        failure: SagaFailure::HandleFailed {
            error: "pre-existing failure".to_owned(),
        },
        occurred_at_unix_millis: 0,
        source: SourceContext::without_upstream(),
    };
    let appending = appending_system_event(1, &pre_existing, jiff::Timestamp::now());
    saga_store
        .append(saga_id.as_str(), vec![appending])
        .await
        .expect("pre-append DeadLetterEvent must succeed");

    // Spawn the subscriber process.
    let received = Arc::new(AtomicUsize::new(0));
    let received_for_proc = Arc::clone(&received);
    let subscriber_proxy = system
        .process_system()
        .spawn(Props::new(move || DlqCollector {
            received: Arc::clone(&received_for_proc),
        }))
        .await;

    // Upstream store with no events — the saga has no upstream events to process;
    // it just needs to be alive so the DLQ subscriber wiring runs.
    let upstream_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());

    let routed = saga_id.clone();
    let _saga_proxy =
        SagaProps::<FailSaga>::new(saga_id.clone(), Arc::clone(&saga_store), || FailSaga)
            .with_codec(system.codec::<SagaLog>())
            .with_dead_letter_subscriber(subscriber_proxy)
            .with_subscription(
                Arc::clone(&upstream_store),
                system.codec::<Ping>(),
                SequenceCursor::Stream {
                    key: "dlq-sub-catchup-upstream".to_owned(),
                    after: 0,
                },
                move |_event: &Ping| Some(routed.clone()),
            )
            .spawn(system.process_system())
            .await;

    let deadline = Instant::now() + Duration::from_secs(6);
    loop {
        if received.load(Ordering::SeqCst) >= 1 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the dead-letter subscriber must catch up and receive the pre-existing \
             DeadLetterEvent within the deadline"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    assert!(
        received.load(Ordering::SeqCst) >= 1,
        "the subscriber must have received the pre-existing dead letter via catchup"
    );
}

/// Regression test for subscriber lifecycle.
///
/// Previously `wire_dead_letter_subscriber` registered a shared `Stream<T>`
/// process under topic `saga-dlq-{saga_id}`.  On saga re-spawn the topic was
/// still in the `ProcessSystem` registry and `DurableStream::spawn()` returned
/// `SpawnError`, silently skipping subscriber wiring and breaking catchup.
///
/// Given a saga stream that already contains a `DeadLetterEvent`,
/// When the saga is spawned, then stopped, then re-spawned with a *new*
/// subscriber proxy (simulating a crash/restart cycle),
/// Then the new subscriber catches up from the EventStore and receives the
/// pre-existing dead letter — proving that saga re-spawn does not break
/// catchup.
#[tokio::test]
async fn dead_letter_subscriber_catchup_survives_saga_respawn() {
    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::new(ps).with_codec::<JsonCodec>().build();

    let saga_id = SagaId::new("dlq-sub-respawn-saga");
    let saga_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());

    // Pre-append a DeadLetterEvent at sequence 1 — simulating a dead letter that
    // was written before the first spawn.
    let pre_existing = DeadLetterEvent {
        seq: 1,
        saga_id: saga_id.clone(),
        failure: SagaFailure::HandleFailed {
            error: "respawn regression failure".to_owned(),
        },
        occurred_at_unix_millis: 0,
        source: SourceContext::without_upstream(),
    };
    let appending = appending_system_event(1, &pre_existing, jiff::Timestamp::now());
    saga_store
        .append(saga_id.as_str(), vec![appending])
        .await
        .expect("pre-append DeadLetterEvent must succeed");

    let upstream_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());

    // --- First spawn ---
    let received_first = Arc::new(AtomicUsize::new(0));
    let received_first_for_proc = Arc::clone(&received_first);
    let subscriber_proxy_first = system
        .process_system()
        .spawn(Props::new(move || DlqCollector {
            received: Arc::clone(&received_first_for_proc),
        }))
        .await;

    let routed = saga_id.clone();
    let saga_proxy_first =
        SagaProps::<FailSaga>::new(saga_id.clone(), Arc::clone(&saga_store), || FailSaga)
            .with_codec(system.codec::<SagaLog>())
            .with_dead_letter_subscriber(subscriber_proxy_first)
            .with_subscription(
                Arc::clone(&upstream_store),
                system.codec::<Ping>(),
                SequenceCursor::Stream {
                    key: "dlq-sub-respawn-upstream".to_owned(),
                    after: 0,
                },
                move |_event: &Ping| Some(routed.clone()),
            )
            .spawn(system.process_system())
            .await;

    let deadline = Instant::now() + Duration::from_secs(6);
    loop {
        if received_first.load(Ordering::SeqCst) >= 1 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "first subscriber must receive the pre-existing DeadLetterEvent"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    // Stop the first saga incarnation.
    saga_proxy_first
        .stop()
        .await
        .expect("first saga stop must succeed");

    // Brief pause to let the runtime fully deregister the first incarnation.
    tokio::time::sleep(Duration::from_millis(100)).await;

    // --- Second spawn (re-spawn) with the same saga id ---
    // This is the regression: previously the saga-dlq-{saga_id} topic was still
    // registered and DurableStream::spawn() would fail, leaving the subscriber
    // unwired.
    let received_second = Arc::new(AtomicUsize::new(0));
    let received_second_for_proc = Arc::clone(&received_second);
    let subscriber_proxy_second = system
        .process_system()
        .spawn(Props::new(move || DlqCollector {
            received: Arc::clone(&received_second_for_proc),
        }))
        .await;

    let routed2 = saga_id.clone();
    let _saga_proxy_second =
        SagaProps::<FailSaga>::new(saga_id.clone(), Arc::clone(&saga_store), || FailSaga)
            .with_codec(system.codec::<SagaLog>())
            .with_dead_letter_subscriber(subscriber_proxy_second)
            .with_subscription(
                Arc::clone(&upstream_store),
                system.codec::<Ping>(),
                SequenceCursor::Stream {
                    key: "dlq-sub-respawn-upstream".to_owned(),
                    after: 0,
                },
                move |_event: &Ping| Some(routed2.clone()),
            )
            .spawn(system.process_system())
            .await;

    let deadline2 = Instant::now() + Duration::from_secs(6);
    loop {
        if received_second.load(Ordering::SeqCst) >= 1 {
            break;
        }
        assert!(
            Instant::now() < deadline2,
            "second subscriber (after re-spawn) must catch up and receive the \
             pre-existing DeadLetterEvent — catchup must hold across re-spawns"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    assert!(
        received_second.load(Ordering::SeqCst) >= 1,
        "catchup must work after saga re-spawn with the same saga id"
    );
}

/// Regression test for subscriber poller identity.
///
/// Previously `spawn_from()` used only `direct_poller_name(subscriber_pid)` to
/// identify the direct poller.  When the same subscriber proxy was registered
/// for saga B after saga A, the B-wiring found the A-poller under that name,
/// stopped it, and spawned a new one pointing at saga B's stream.  Saga A's DLQ
/// catchup silently stopped.
///
/// The fix encodes `subscriber_pid + saga_id` in the poller name so each
/// (subscriber, saga) pair gets its own independent direct poller.
///
/// Given two different sagas, each with a pre-existing `DeadLetterEvent`,
/// When the SAME subscriber proxy is registered for both sagas,
/// Then both DLQ events must be delivered to the subscriber — neither saga's
/// poller must be stopped by the other saga's wiring.
#[tokio::test]
async fn same_subscriber_registered_for_two_sagas_receives_both_dead_letters() {
    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::new(ps).with_codec::<JsonCodec>().build();

    // --- Pre-populate saga A stream ---
    let saga_id_a = SagaId::new("dlq-multi-saga-a");
    let saga_store_a: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let pre_a = DeadLetterEvent {
        seq: 1,
        saga_id: saga_id_a.clone(),
        failure: SagaFailure::HandleFailed {
            error: "saga-a failure".to_owned(),
        },
        occurred_at_unix_millis: 0,
        source: SourceContext::without_upstream(),
    };
    saga_store_a
        .append(
            saga_id_a.as_str(),
            vec![appending_system_event(1, &pre_a, jiff::Timestamp::now())],
        )
        .await
        .expect("pre-append to saga A must succeed");

    // --- Pre-populate saga B stream ---
    let saga_id_b = SagaId::new("dlq-multi-saga-b");
    let saga_store_b: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let pre_b = DeadLetterEvent {
        seq: 1,
        saga_id: saga_id_b.clone(),
        failure: SagaFailure::HandleFailed {
            error: "saga-b failure".to_owned(),
        },
        occurred_at_unix_millis: 0,
        source: SourceContext::without_upstream(),
    };
    saga_store_b
        .append(
            saga_id_b.as_str(),
            vec![appending_system_event(1, &pre_b, jiff::Timestamp::now())],
        )
        .await
        .expect("pre-append to saga B must succeed");

    // --- Spawn ONE shared subscriber process ---
    let received = Arc::new(AtomicUsize::new(0));
    let received_for_proc = Arc::clone(&received);
    let subscriber_proxy = system
        .process_system()
        .spawn(Props::new(move || DlqCollector {
            received: Arc::clone(&received_for_proc),
        }))
        .await;

    let upstream_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());

    // --- Wire subscriber to saga A (first) ---
    let routed_a = saga_id_a.clone();
    let _saga_a =
        SagaProps::<FailSaga>::new(saga_id_a.clone(), Arc::clone(&saga_store_a), || FailSaga)
            .with_codec(system.codec::<SagaLog>())
            .with_dead_letter_subscriber(subscriber_proxy.clone())
            .with_subscription(
                Arc::clone(&upstream_store),
                system.codec::<Ping>(),
                SequenceCursor::Stream {
                    key: "dlq-multi-upstream-a".to_owned(),
                    after: 0,
                },
                move |_event: &Ping| Some(routed_a.clone()),
            )
            .spawn(system.process_system())
            .await;

    // --- Wire SAME subscriber to saga B (second) ---
    // Previously, wiring for B would stop the A-poller because both shared the
    // same process name derived from subscriber_pid alone.
    let routed_b = saga_id_b.clone();
    let _saga_b =
        SagaProps::<FailSaga>::new(saga_id_b.clone(), Arc::clone(&saga_store_b), || FailSaga)
            .with_codec(system.codec::<SagaLog>())
            .with_dead_letter_subscriber(subscriber_proxy.clone())
            .with_subscription(
                Arc::clone(&upstream_store),
                system.codec::<Ping>(),
                SequenceCursor::Stream {
                    key: "dlq-multi-upstream-b".to_owned(),
                    after: 0,
                },
                move |_event: &Ping| Some(routed_b.clone()),
            )
            .spawn(system.process_system())
            .await;

    // Both pre-existing DLQ events (one from each saga) must reach the subscriber.
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        if received.load(Ordering::SeqCst) >= 2 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the shared subscriber must receive DLQ events from BOTH saga A and saga B \
             within the deadline — each saga must have its own independent direct poller"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    assert!(
        received.load(Ordering::SeqCst) >= 2,
        "subscriber must receive at least 2 dead letters (one from each saga); \
         got {} — saga B wiring must not have stopped saga A's poller",
        received.load(Ordering::SeqCst)
    );
}

/// Regression test for subscriber lifecycle.
///
/// The DLQ direct-poller must be bound to the **saga** lifecycle, not just to
/// the subscriber's lifecycle.  Previously the poller was spawned as a
/// top-level process via `spawn_from()` and only watched the subscriber for
/// termination.  Stopping the saga had no effect on the poller — it kept
/// running as long as the subscriber was alive, polling a stream that belonged
/// to a stopped saga.
///
/// The fix spawns the poller as a **child** of the saga from
/// `SagaProcess::on_start`: when the saga stops the
/// runtime's child cascade stops the poller automatically.
///
/// Given a saga with a DLQ subscriber wired, and the subscriber still alive,
/// When the saga is stopped,
/// Then the DLQ direct-poller (`dlq-poller-{subscriber_pid}-{saga_id}`) must
/// no longer appear in the process registry.
#[tokio::test]
async fn dlq_poller_is_stopped_when_saga_stops_while_subscriber_stays_alive() {
    use nitinol_runtime::ident::ProcessName;

    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::new(ps).with_codec::<JsonCodec>().build();

    let saga_id = SagaId::new("dlq-lifecycle-saga");
    let saga_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let upstream_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());

    // Spawn the subscriber and keep it alive for the whole test.
    let received = Arc::new(AtomicUsize::new(0));
    let received_for_proc = Arc::clone(&received);
    let subscriber_proxy = system
        .process_system()
        .spawn(Props::new(move || DlqCollector {
            received: Arc::clone(&received_for_proc),
        }))
        .await;
    let subscriber_pid = subscriber_proxy.pid();

    let routed = saga_id.clone();
    let saga_proxy =
        SagaProps::<FailSaga>::new(saga_id.clone(), Arc::clone(&saga_store), || FailSaga)
            .with_codec(system.codec::<SagaLog>())
            .with_dead_letter_subscriber(subscriber_proxy.clone())
            .with_subscription(
                Arc::clone(&upstream_store),
                system.codec::<Ping>(),
                SequenceCursor::Stream {
                    key: "dlq-lifecycle-upstream".to_owned(),
                    after: 0,
                },
                move |_event: &Ping| Some(routed.clone()),
            )
            .spawn(system.process_system())
            .await;

    // The DLQ poller name uses the format established by dlq_poller_name():
    // "dlq-poller-{subscriber_pid}-{saga_id}".
    let poller_name = ProcessName::new(format!(
        "dlq-poller-{}-{}",
        subscriber_pid,
        saga_id.as_str()
    ));

    // Wait until the DLQ poller is visible in the registry (on_start has run).
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if system
            .process_system()
            .lookup_by_name(&poller_name)
            .await
            .is_some()
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "DLQ poller must appear in registry after saga starts"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    // Stop the saga.  The subscriber remains alive.
    saga_proxy.stop().await.expect("saga stop must succeed");

    // The DLQ poller must disappear from the registry once the saga's child
    // cascade completes.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if system
            .process_system()
            .lookup_by_name(&poller_name)
            .await
            .is_none()
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "DLQ poller must be removed from registry after saga stops \
             (the poller must be bound to the saga lifecycle, not the subscriber lifecycle)"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    // The subscriber must still be alive — only the saga and its children stopped.
    let subscriber_alive = system
        .process_system()
        .lookup(subscriber_pid)
        .await
        .is_some();
    assert!(
        subscriber_alive,
        "subscriber must remain alive after saga stops — only the saga's child cascade runs"
    );
}
