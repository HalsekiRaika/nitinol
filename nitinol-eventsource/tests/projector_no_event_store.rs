use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use tokio::sync::Notify;

use nitinol_eventsource::{
    codec::Codec,
    Event, ProjectionContext, Projector, ProjectorProps,
    EventPersistor, EventPersistorProxy,
};
use nitinol_persistence::store::{EventStore, InMemoryCheckpointStore, InMemoryEventStore};
use nitinol_persistence::{AggregateId, AppendingEvent, EventType, ProjectionId};
use nitinol_runtime::ProcessSystem;

// ---------------------------------------------------------------------------
// Fixtures: minimal event type
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct PingEvent;

impl Event for PingEvent {
    const EVENT_TYPE: EventType = EventType::from_str("PingEvent");
}

// ---------------------------------------------------------------------------
// Fixtures: codec
// ---------------------------------------------------------------------------

struct PingCodec;

impl Codec<PingEvent> for PingCodec {
    type Error = std::convert::Infallible;

    fn encode(_event: &PingEvent) -> Result<Bytes, Self::Error> {
        Ok(Bytes::new())
    }

    fn decode(_payload: &[u8]) -> Result<PingEvent, Self::Error> {
        Ok(PingEvent)
    }
}

// ---------------------------------------------------------------------------
// Fixtures: FlagProjector
// ---------------------------------------------------------------------------

struct FlagProjector {
    called: Arc<AtomicBool>,
    notify: Arc<Notify>,
}

impl FlagProjector {
    fn new_flagged(called: Arc<AtomicBool>, notify: Arc<Notify>) -> Self {
        FlagProjector { called, notify }
    }
}

#[async_trait]
impl Projector<PingEvent> for FlagProjector {
    type Error = std::convert::Infallible;

    async fn project(
        &mut self,
        _event: PingEvent,
        _ctx: &mut ProjectionContext<'_, ()>,
    ) -> Result<(), Self::Error> {
        self.called.store(true, Ordering::SeqCst);
        self.notify.notify_one();
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Compile-time type check
// ---------------------------------------------------------------------------

/// Asserts that ProjectorProps::new accepts EventPersistorProxy as its second argument.
///
/// This is NOT a #[test] function — it is never executed at runtime.
/// If the second parameter type regresses to Arc<dyn EventStore>, passing an
/// EventPersistorProxy here causes a compile error, surfacing the regression
/// immediately at build time.
fn _assert_projector_props_new_accepts_event_persistor_proxy(
    pid: ProjectionId,
    event_ref: EventPersistorProxy,
    cs: Arc<InMemoryCheckpointStore>,
) {
    let called = Arc::new(AtomicBool::new(false));
    let notify = Arc::new(Notify::new());
    let called_c = Arc::clone(&called);
    let notify_c = Arc::clone(&notify);
    // Compile error here if ProjectorProps::new's 2nd param is Arc<dyn EventStore>
    let _ = ProjectorProps::new(pid, event_ref, cs, move || {
        FlagProjector::new_flagged(Arc::clone(&called_c), Arc::clone(&notify_c))
    });
}

// ---------------------------------------------------------------------------
// Runtime integration: catch-up via EventPersistorProxy succeeds
// ---------------------------------------------------------------------------

/// Projector spawned via EventPersistorProxy processes one stored event during catch-up.
#[tokio::test]
async fn projector_completes_catchup_via_event_persistor_proxy() {
    // Given
    let system = ProcessSystem::new().await;
    let store = Arc::new(InMemoryEventStore::default());
    let checkpoint_store = Arc::new(InMemoryCheckpointStore::default());
    let agg_id = AggregateId::new("projector-no-store-catchup");

    // Pre-load one event so catch-up has something to process.
    store
        .append(
            &agg_id,
            vec![AppendingEvent {
                aggregate_id: agg_id.clone(),
                sequence: 1,
                event_type: EventType::from_str("PingEvent"),
                payload: Bytes::new(),
                occurred_at: jiff::Timestamp::now(),
            }],
        )
        .await
        .expect("append must succeed");

    // Spawn EventPersistor wrapping the store — ProjectorProps gets the proxy only.
    // Arc<InMemoryEventStore> coerces implicitly to Arc<dyn EventStore>.
    let event_ref = EventPersistor::spawn(&system, store).await;

    let called = Arc::new(AtomicBool::new(false));
    let notify = Arc::new(Notify::new());
    let called_c = Arc::clone(&called);
    let notify_c = Arc::clone(&notify);

    // When: spawn projector with EventPersistorProxy (not Arc<dyn EventStore>)
    let _proxy = ProjectorProps::new(
        ProjectionId::new("projector-no-store-id"),
        event_ref,                       // EventPersistorProxy — the key assertion
        Arc::clone(&checkpoint_store),
        move || FlagProjector::new_flagged(Arc::clone(&called_c), Arc::clone(&notify_c)),
    )
    .with_event::<PingEvent>(Arc::new(PingCodec))
    .catchup_from_aggregate(agg_id)
    .spawn(&system)
    .await;

    // Then: project() is called during catch-up within the timeout
    tokio::time::timeout(Duration::from_millis(500), async {
        loop {
            let notified = notify.notified();
            if called.load(Ordering::SeqCst) {
                return;
            }
            notified.await;
        }
    })
    .await
    .expect("project() must be called within 500 ms via EventPersistorProxy catch-up");

    assert!(
        called.load(Ordering::SeqCst),
        "FlagProjector::project must have been called during catch-up via EventPersistorProxy"
    );
}

// ---------------------------------------------------------------------------
// Runtime integration: multiple events processed sequentially
// ---------------------------------------------------------------------------

/// Projector via EventPersistorProxy correctly processes multiple sequential events.
///
/// This ensures that the Vec<LoadedEvent> returned by EventPersistorProxy::load
/// is iterated completely, not just the first element.
#[tokio::test]
async fn projector_processes_multiple_events_via_event_persistor_proxy() {
    // Given
    let system = ProcessSystem::new().await;
    let store = Arc::new(InMemoryEventStore::default());
    let checkpoint_store = Arc::new(InMemoryCheckpointStore::default());
    let agg_id = AggregateId::new("projector-no-store-multi");

    for seq in 1u64..=3 {
        store
            .append(
                &agg_id,
                vec![AppendingEvent {
                    aggregate_id: agg_id.clone(),
                    sequence: seq,
                    event_type: EventType::from_str("PingEvent"),
                    payload: Bytes::new(),
                    occurred_at: jiff::Timestamp::now(),
                }],
            )
            .await
            .expect("append must succeed");
    }

    let event_ref = EventPersistor::spawn(&system, store).await;

    use std::sync::atomic::AtomicUsize;
    let count = Arc::new(AtomicUsize::new(0));
    let notify = Arc::new(Notify::new());
    let count_c = Arc::clone(&count);
    let notify_c = Arc::clone(&notify);

    // Projector that counts calls instead of flagging
    struct CountingProjector {
        count: Arc<AtomicUsize>,
        notify: Arc<Notify>,
    }

    #[async_trait]
    impl Projector<PingEvent> for CountingProjector {
        type Error = std::convert::Infallible;

        async fn project(
            &mut self,
            _event: PingEvent,
            _ctx: &mut ProjectionContext<'_, ()>,
        ) -> Result<(), Self::Error> {
            self.count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.notify.notify_one();
            Ok(())
        }
    }

    // When
    let _proxy = ProjectorProps::new(
        ProjectionId::new("projector-no-store-multi-id"),
        event_ref,
        Arc::clone(&checkpoint_store),
        move || CountingProjector {
            count: Arc::clone(&count_c),
            notify: Arc::clone(&notify_c),
        },
    )
    .with_event::<PingEvent>(Arc::new(PingCodec))
    .catchup_from_aggregate(agg_id)
    .spawn(&system)
    .await;

    // Then: all three events are projected
    tokio::time::timeout(Duration::from_millis(500), async {
        loop {
            let notified = notify.notified();
            if count.load(Ordering::SeqCst) >= 3 {
                return;
            }
            notified.await;
        }
    })
    .await
    .expect("all 3 events must be projected within 500 ms");

    assert_eq!(
        count.load(Ordering::SeqCst),
        3,
        "project() must be called once per stored event (3 total)"
    );
}
