// E2E test: Checkpoint delivery modes for ProjectorProcess.
//
// Scenario: ProjectorProcess + CheckpointStore, three delivery modes.
//
// Three E2E stories:
//   1. AtLeastOnce  — failed projection is retried on restart because the
//      checkpoint was NOT saved (the event is reprocessed after restart).
//   2. ExactlyOnce  — the user saves both the read-model update AND the
//      checkpoint inside project(). On restart, no events are reprocessed.
//   3. AtMostOnce   — the checkpoint is saved BEFORE project() so a failed
//      projection is treated as "delivered" (no reprocessing on restart).

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use tokio::sync::Notify;

use nitinol_eventsource::{Event, EventPersistor, ProjectionContext, Projector, ProjectorProps};
use nitinol_eventsource::codec::Codec;
use nitinol_persistence::store::{CheckpointStore, DeliveryMode, EventStore, InMemoryCheckpointStore, InMemoryEventStore};
use nitinol_persistence::{AggregateId, AppendingEvent, EventType, ProjectionId};
use nitinol_runtime::ProcessSystem;

// ---------------------------------------------------------------------------
// Fixtures: event
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct Evt;

impl Event for Evt {
    const EVENT_TYPE: EventType = EventType::from_str("E2ECkpt.Evt");
}

// ---------------------------------------------------------------------------
// Fixtures: pass-through codec
// ---------------------------------------------------------------------------

struct UnitCodec;

impl Codec<Evt> for UnitCodec {
    type Error = std::convert::Infallible;

    fn encode(_event: &Evt) -> Result<Bytes, Self::Error> {
        Ok(Bytes::new())
    }

    fn decode(_payload: &[u8]) -> Result<Evt, Self::Error> {
        Ok(Evt)
    }
}

// ---------------------------------------------------------------------------
// Fixtures: ConditionallyFailingProjector
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
#[error("intentional projection failure")]
struct ProjectFailed;

struct ConditionallyFailingProjector {
    should_fail: Arc<AtomicBool>,
    count: Arc<AtomicUsize>,
    notify: Arc<Notify>,
}

#[async_trait]
impl Projector<Evt> for ConditionallyFailingProjector {
    type Error = ProjectFailed;

    async fn project(
        &mut self,
        _event: Evt,
        _ctx: &mut ProjectionContext<'_, ()>,
    ) -> Result<(), Self::Error> {
        self.count.fetch_add(1, Ordering::SeqCst);
        self.notify.notify_one();
        if self.should_fail.load(Ordering::SeqCst) {
            Err(ProjectFailed)
        } else {
            Ok(())
        }
    }
}

// ---------------------------------------------------------------------------
// Fixtures: ExactlyOnceProjector — user saves checkpoint inside project()
// ---------------------------------------------------------------------------

struct ExactlyOnceProjector {
    /// Direct reference to the checkpoint store so the user can save atomically.
    checkpoint_store: Arc<InMemoryCheckpointStore>,
    projection_id: ProjectionId,
    count: Arc<AtomicUsize>,
    notify: Arc<Notify>,
}

#[async_trait]
impl Projector<Evt> for ExactlyOnceProjector {
    type Error = std::convert::Infallible;

    async fn project(
        &mut self,
        _event: Evt,
        ctx: &mut ProjectionContext<'_, ()>,
    ) -> Result<(), Self::Error> {
        // Simulate an atomic "read-model update + checkpoint save" in a single TX.
        // For InMemory stores this is sequential, not truly atomic, but demonstrates
        // the ExactlyOnce user pattern.
        self.checkpoint_store
            .save(&self.projection_id, ctx.current_sequence(), None)
            .await
            .expect("user checkpoint save must succeed");
        self.count.fetch_add(1, Ordering::SeqCst);
        self.notify.notify_one();
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Append one Evt to the event store at the given sequence number.
async fn append_evt(store: &InMemoryEventStore, agg_id: &AggregateId, sequence: u64) {
    store
        .append(
            agg_id,
            vec![AppendingEvent {
                aggregate_id: agg_id.clone(),
                sequence,
                event_type: EventType::from_str("E2ECkpt.Evt"),
                payload: Bytes::new(),
                occurred_at: jiff::Timestamp::now(),
            }],
        )
        .await
        .expect("append must succeed");
}

/// Poll the checkpoint store until it reaches `expected` or until the timeout elapses.
async fn wait_for_checkpoint(
    store: &Arc<InMemoryCheckpointStore>,
    projection_id: &ProjectionId,
    expected: Option<u64>,
) {
    tokio::time::timeout(Duration::from_millis(500), async {
        loop {
            let current = store
                .load(projection_id)
                .await
                .expect("checkpoint load must succeed");
            if current == expected {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!("timed out waiting for checkpoint to reach {:?}", expected)
    });
}

/// Poll until the counter reaches `expected` or the timeout elapses.
async fn wait_for_count(counter: &Arc<AtomicUsize>, notify: &Arc<Notify>, expected: usize) {
    tokio::time::timeout(Duration::from_millis(500), async {
        loop {
            let notified = notify.notified();
            if counter.load(Ordering::SeqCst) >= expected {
                return;
            }
            notified.await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {expected} project() calls"));
}

// ---------------------------------------------------------------------------
// Test 1: AtLeastOnce — failed event retried on restart
// ---------------------------------------------------------------------------

/// Given two events (seq=1, seq=2) and an AtLeastOnce projector that fails on seq=2,
/// When the projector is restarted,
/// Then seq=2 is reprocessed (at-least-once guarantee) and the checkpoint reaches 2.
///
/// Steps:
///   First run:  seq=1 → ok (checkpoint=1), seq=2 → fail (checkpoint stays=1)
///   Restart:    checkpoint=1 → start from seq=2 → succeeds → checkpoint=2
#[tokio::test(flavor = "multi_thread")]
async fn e2e_at_least_once_failed_projection_retried_on_restart() {
    // Given
    let system = ProcessSystem::new().await;
    let event_store = Arc::new(InMemoryEventStore::default());
    let checkpoint_store = Arc::new(InMemoryCheckpointStore::default());
    let agg_id = AggregateId::new("e2e-ckpt-alo-agg");
    let projection_id = ProjectionId::new("e2e-ckpt-alo");

    append_evt(&event_store, &agg_id, 1).await;
    append_evt(&event_store, &agg_id, 2).await;

    let should_fail = Arc::new(AtomicBool::new(false));
    let count = Arc::new(AtomicUsize::new(0));
    let notify = Arc::new(Notify::new());

    // First run: succeed on seq=1, fail on seq=2
    {
        let sf_c = Arc::clone(&should_fail);
        let count_c = Arc::clone(&count);
        let notify_c = Arc::clone(&notify);

        let event_ref = EventPersistor::spawn(&system, Arc::clone(&event_store) as Arc<dyn nitinol_persistence::store::EventStore>).await;
        let _proxy = ProjectorProps::new(
            projection_id.clone(),
            event_ref,
            Arc::clone(&checkpoint_store),
            move || ConditionallyFailingProjector {
                should_fail: Arc::clone(&sf_c),
                count: Arc::clone(&count_c),
                notify: Arc::clone(&notify_c),
            },
        )
        .with_event::<Evt>(Arc::new(UnitCodec))
        .delivery_mode(DeliveryMode::AtLeastOnce)
        .catchup_from_aggregate(agg_id.clone())
        .spawn(&system)
        .await;

        // Wait for seq=1 to succeed, then trip the failure flag before seq=2
        wait_for_count(&count, &notify, 1).await;
        should_fail.store(true, Ordering::SeqCst);

        // Wait for the seq=2 failure attempt
        wait_for_count(&count, &notify, 2).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // Verify checkpoint is at seq=1 (seq=2 was not saved due to failure)
    let ckpt_after_first = checkpoint_store
        .load(&projection_id)
        .await
        .expect("load must succeed");
    assert_eq!(
        ckpt_after_first,
        Some(1),
        "after first run: checkpoint must be seq=1 (seq=2 failed)"
    );

    // When: restart with failure disabled
    should_fail.store(false, Ordering::SeqCst);
    count.store(0, Ordering::SeqCst);

    // Clone before the move closure so the originals are available for assertions.
    let sf2 = Arc::clone(&should_fail);
    let count2 = Arc::clone(&count);
    let notify2 = Arc::clone(&notify);

    let event_ref2 = EventPersistor::spawn(&system, Arc::clone(&event_store) as Arc<dyn nitinol_persistence::store::EventStore>).await;
    let _proxy2 = ProjectorProps::new(
        projection_id.clone(),
        event_ref2,
        Arc::clone(&checkpoint_store),
        move || ConditionallyFailingProjector {
            should_fail: Arc::clone(&sf2),
            count: Arc::clone(&count2),
            notify: Arc::clone(&notify2),
        },
    )
    .with_event::<Evt>(Arc::new(UnitCodec))
    .delivery_mode(DeliveryMode::AtLeastOnce)
    .catchup_from_aggregate(agg_id)
    .spawn(&system)
    .await;

    // seq=2 is reprocessed on restart
    wait_for_count(&count, &notify, 1).await;

    // Then: checkpoint advances to seq=2 after successful reprocessing
    wait_for_checkpoint(&checkpoint_store, &projection_id, Some(2)).await;
}

// ---------------------------------------------------------------------------
// Test 2: ExactlyOnce — user saves checkpoint inside project(); no reprocessing on restart
// ---------------------------------------------------------------------------

/// Given two events and an ExactlyOnce projector that saves its own checkpoint
/// inside project() (simulating an atomic read-model + checkpoint TX),
/// When the projector is restarted,
/// Then no events are reprocessed (checkpoint=2 → start from seq=3 → 0 events).
#[tokio::test]
async fn e2e_exactly_once_user_saves_checkpoint_prevents_reprocessing() {
    // Given
    let system = ProcessSystem::new().await;
    let event_store = Arc::new(InMemoryEventStore::default());
    let checkpoint_store = Arc::new(InMemoryCheckpointStore::default());
    let agg_id = AggregateId::new("e2e-ckpt-eo-agg");
    let projection_id = ProjectionId::new("e2e-ckpt-eo");

    append_evt(&event_store, &agg_id, 1).await;
    append_evt(&event_store, &agg_id, 2).await;

    let count = Arc::new(AtomicUsize::new(0));
    let notify = Arc::new(Notify::new());

    // First run: process both events, user saves checkpoint inside project()
    {
        let count_c = Arc::clone(&count);
        let notify_c = Arc::clone(&notify);
        let ckpt_c = Arc::clone(&checkpoint_store);
        let proj_id_c = projection_id.clone();

        let event_ref = EventPersistor::spawn(&system, Arc::clone(&event_store) as Arc<dyn nitinol_persistence::store::EventStore>).await;
        let _proxy = ProjectorProps::new(
            projection_id.clone(),
            event_ref,
            Arc::clone(&checkpoint_store),
            move || ExactlyOnceProjector {
                checkpoint_store: Arc::clone(&ckpt_c),
                projection_id: proj_id_c.clone(),
                count: Arc::clone(&count_c),
                notify: Arc::clone(&notify_c),
            },
        )
        .with_event::<Evt>(Arc::new(UnitCodec))
        .delivery_mode(DeliveryMode::ExactlyOnce)
        .catchup_from_aggregate(agg_id.clone())
        .spawn(&system)
        .await;

        wait_for_count(&count, &notify, 2).await;
    }

    // Checkpoint was saved by the user inside project()
    let ckpt_after_first = checkpoint_store
        .load(&projection_id)
        .await
        .expect("load must succeed");
    assert_eq!(
        ckpt_after_first,
        Some(2),
        "user must have saved checkpoint=2 inside project()"
    );

    // When: restart the projector — checkpoint=2, no events after seq=2
    count.store(0, Ordering::SeqCst);

    let count_c2 = Arc::clone(&count);
    let notify_c2 = Arc::clone(&notify);
    let ckpt_c2 = Arc::clone(&checkpoint_store);
    let proj_id_c2 = projection_id.clone();

    let event_ref2 = EventPersistor::spawn(&system, Arc::clone(&event_store) as Arc<dyn nitinol_persistence::store::EventStore>).await;
    let _proxy2 = ProjectorProps::new(
        projection_id.clone(),
        event_ref2,
        Arc::clone(&checkpoint_store),
        move || ExactlyOnceProjector {
            checkpoint_store: Arc::clone(&ckpt_c2),
            projection_id: proj_id_c2.clone(),
            count: Arc::clone(&count_c2),
            notify: Arc::clone(&notify_c2),
        },
    )
    .with_event::<Evt>(Arc::new(UnitCodec))
    .delivery_mode(DeliveryMode::ExactlyOnce)
    .catchup_from_aggregate(agg_id)
    .spawn(&system)
    .await;

    // Give the projector time to complete any catch-up (should be empty)
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Then: no events reprocessed on restart — exactly-once achieved
    assert_eq!(
        count.load(Ordering::SeqCst),
        0,
        "on restart with checkpoint=2, no events must be reprocessed (exactly-once)"
    );
}

// ---------------------------------------------------------------------------
// Test 3: AtMostOnce — failed event is NOT retried on restart
// ---------------------------------------------------------------------------

/// Given one event (seq=1) and an AtMostOnce projector whose project() fails,
/// When the projector is restarted,
/// Then no events are reprocessed (checkpoint was saved BEFORE the failing project() call).
#[tokio::test]
async fn e2e_at_most_once_failed_event_not_retried_on_restart() {
    // Given
    let system = ProcessSystem::new().await;
    let event_store = Arc::new(InMemoryEventStore::default());
    let checkpoint_store = Arc::new(InMemoryCheckpointStore::default());
    let agg_id = AggregateId::new("e2e-ckpt-amo-agg");
    let projection_id = ProjectionId::new("e2e-ckpt-amo");

    append_evt(&event_store, &agg_id, 1).await;

    let should_fail = Arc::new(AtomicBool::new(true)); // always fail
    let count = Arc::new(AtomicUsize::new(0));
    let notify = Arc::new(Notify::new());

    // First run: AtMostOnce saves checkpoint BEFORE calling project(), then project() fails
    {
        let sf_c = Arc::clone(&should_fail);
        let count_c = Arc::clone(&count);
        let notify_c = Arc::clone(&notify);

        let event_ref = EventPersistor::spawn(&system, Arc::clone(&event_store) as Arc<dyn nitinol_persistence::store::EventStore>).await;
        let _proxy = ProjectorProps::new(
            projection_id.clone(),
            event_ref,
            Arc::clone(&checkpoint_store),
            move || ConditionallyFailingProjector {
                should_fail: Arc::clone(&sf_c),
                count: Arc::clone(&count_c),
                notify: Arc::clone(&notify_c),
            },
        )
        .with_event::<Evt>(Arc::new(UnitCodec))
        .delivery_mode(DeliveryMode::AtMostOnce)
        .catchup_from_aggregate(agg_id.clone())
        .spawn(&system)
        .await;

        wait_for_count(&count, &notify, 1).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // Checkpoint IS at seq=1 (saved before project() was called — AtMostOnce)
    let ckpt_after_first = checkpoint_store
        .load(&projection_id)
        .await
        .expect("load must succeed");
    assert_eq!(
        ckpt_after_first,
        Some(1),
        "AtMostOnce: checkpoint must be seq=1 even though project() failed"
    );

    // When: restart the projector — checkpoint=1, no events after seq=1
    should_fail.store(false, Ordering::SeqCst);
    count.store(0, Ordering::SeqCst);

    // Clone before the move closure so the originals remain accessible for assertions.
    let sf2 = Arc::clone(&should_fail);
    let count2 = Arc::clone(&count);
    let notify2 = Arc::clone(&notify);

    let event_ref2 = EventPersistor::spawn(&system, Arc::clone(&event_store) as Arc<dyn nitinol_persistence::store::EventStore>).await;
    let _proxy2 = ProjectorProps::new(
        projection_id,
        event_ref2,
        Arc::clone(&checkpoint_store),
        move || ConditionallyFailingProjector {
            should_fail: Arc::clone(&sf2),
            count: Arc::clone(&count2),
            notify: Arc::clone(&notify2),
        },
    )
    .with_event::<Evt>(Arc::new(UnitCodec))
    .delivery_mode(DeliveryMode::AtMostOnce)
    .catchup_from_aggregate(agg_id)
    .spawn(&system)
    .await;

    // Give the projector time to complete any catch-up (should be empty)
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Then: no events reprocessed — AtMostOnce skipped the failed event
    assert_eq!(
        count.load(Ordering::SeqCst),
        0,
        "AtMostOnce: failed event must NOT be retried on restart (checkpoint was already advanced)"
    );
}
