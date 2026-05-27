// Integration tests for DeliveryMode (AtMostOnce / AtLeastOnce / ExactlyOnce).
//
// Tests verify the checkpoint-save ordering relative to project() for each mode,
// and the exact semantics when project() fails.
//
// AtMostOnce:
//   checkpoint is saved BEFORE project(). If project() fails, the checkpoint
//   is already advanced — the event will NOT be re-processed on restart.
//
// AtLeastOnce (default):
//   checkpoint is saved AFTER a successful project(). If project() fails, the
//   checkpoint is NOT advanced — the event WILL be re-processed on restart.
//
// ExactlyOnce:
//   the framework does NOT save the checkpoint at all. The user is responsible
//   for saving both the read-model update and the checkpoint inside project()
//   within the same transaction.
//
// Compile errors involving the projection module are expected until the
// implementation in nitinol-eventsource/src/projection/ is complete.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use tokio::sync::Notify;

use nitinol_eventsource::{codec::Codec, Event, ProjectionContext, Projector, ProjectorProps};
use nitinol_persistence::store::{
    CheckpointStore, DeliveryMode, EventStore, InMemoryCheckpointStore, InMemoryEventStore,
};
use nitinol_persistence::{AggregateId, AppendingEvent, EventType, ProjectionId};
use nitinol_runtime::ProcessSystem;

// ---------------------------------------------------------------------------
// Fixtures: event type
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct Evt;

impl Event for Evt {
    const EVENT_TYPE: EventType = EventType::from_str("Evt");
}

// ---------------------------------------------------------------------------
// Fixtures: codec
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
//
// When `should_fail` is true, project() returns an error.
// When false, project() succeeds.
// The call count and notify are shared so tests can observe execution.
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
#[error("intentional project failure")]
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
// Helpers
// ---------------------------------------------------------------------------

async fn append_evt(store: &InMemoryEventStore, agg_id: &AggregateId, sequence: u64) {
    store
        .append(
            agg_id.as_str(),
            vec![AppendingEvent {
                sequence,
                event_type: EventType::from_str("Evt"),
                payload: Bytes::new(),
                occurred_at: jiff::Timestamp::now(),
            }],
        )
        .await
        .expect("append must succeed");
}

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

/// Poll the checkpoint store until it reaches the expected value or timeout.
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
    .unwrap_or_else(|_| panic!("timed out waiting for checkpoint to reach {:?}", expected));
}

// ---------------------------------------------------------------------------
// Test: AtMostOnce — checkpoint is advanced even when project() fails
// ---------------------------------------------------------------------------

/// With AtMostOnce, the checkpoint is saved BEFORE calling project().
/// If project() fails, the checkpoint has already advanced — the event is
/// treated as "processed" (at most once delivery) even though it was not
/// actually processed successfully.
#[tokio::test]
async fn at_most_once_checkpoint_advanced_even_when_project_fails() {
    // Given
    let system = ProcessSystem::new().await;
    let event_store = Arc::new(InMemoryEventStore::default());
    let checkpoint_store = Arc::new(InMemoryCheckpointStore::default());
    let agg_id = AggregateId::new("dm-amo-agg");
    let projection_id = ProjectionId::new("dm-amo");

    append_evt(&event_store, &agg_id, 1).await;

    let should_fail = Arc::new(AtomicBool::new(true)); // always fail
    let count = Arc::new(AtomicUsize::new(0));
    let notify = Arc::new(Notify::new());
    let sf_c = Arc::clone(&should_fail);
    let count_c = Arc::clone(&count);
    let notify_c = Arc::clone(&notify);

    // When: spawn with AtMostOnce
    let _proxy = ProjectorProps::new(
        projection_id.clone(),
        Arc::clone(&event_store) as Arc<dyn EventStore>,
        Arc::clone(&checkpoint_store),
        move || ConditionallyFailingProjector {
            should_fail: Arc::clone(&sf_c),
            count: Arc::clone(&count_c),
            notify: Arc::clone(&notify_c),
        },
    )
    .with_event::<Evt>(Arc::new(UnitCodec))
    .delivery_mode(DeliveryMode::AtMostOnce)
    .catchup_from_aggregate(agg_id)
    .spawn(&system)
    .await;

    // Wait for project() to be called
    wait_for_count(&count, &notify, 1).await;

    // AtMostOnce saves the checkpoint BEFORE calling project(). When project() fires the
    // Notify, the checkpoint is already saved.
    // Then: checkpoint is at seq=1 despite project() failing
    wait_for_checkpoint(&checkpoint_store, &projection_id, Some(1)).await;

    assert_eq!(
        count.load(Ordering::SeqCst),
        1,
        "project() must have been called once"
    );
}

// ---------------------------------------------------------------------------
// Test: AtMostOnce — checkpoint advances on success too
// ---------------------------------------------------------------------------

/// With AtMostOnce, a successful project() also results in the checkpoint
/// being saved (it was saved before the call).
#[tokio::test]
async fn at_most_once_checkpoint_advanced_on_success() {
    // Given
    let system = ProcessSystem::new().await;
    let event_store = Arc::new(InMemoryEventStore::default());
    let checkpoint_store = Arc::new(InMemoryCheckpointStore::default());
    let agg_id = AggregateId::new("dm-amo-ok-agg");
    let projection_id = ProjectionId::new("dm-amo-ok");

    for seq in 1..=3 {
        append_evt(&event_store, &agg_id, seq).await;
    }

    let should_fail = Arc::new(AtomicBool::new(false)); // always succeed
    let count = Arc::new(AtomicUsize::new(0));
    let notify = Arc::new(Notify::new());
    let sf_c = Arc::clone(&should_fail);
    let count_c = Arc::clone(&count);
    let notify_c = Arc::clone(&notify);

    // When
    let _proxy = ProjectorProps::new(
        projection_id.clone(),
        Arc::clone(&event_store) as Arc<dyn EventStore>,
        Arc::clone(&checkpoint_store),
        move || ConditionallyFailingProjector {
            should_fail: Arc::clone(&sf_c),
            count: Arc::clone(&count_c),
            notify: Arc::clone(&notify_c),
        },
    )
    .with_event::<Evt>(Arc::new(UnitCodec))
    .delivery_mode(DeliveryMode::AtMostOnce)
    .catchup_from_aggregate(agg_id)
    .spawn(&system)
    .await;

    // Wait for all 3 project() calls, then for checkpoint to settle at 3
    wait_for_count(&count, &notify, 3).await;

    // Then: checkpoint reaches 3
    wait_for_checkpoint(&checkpoint_store, &projection_id, Some(3)).await;
}

// ---------------------------------------------------------------------------
// Test: AtLeastOnce — checkpoint NOT saved when project() fails
// ---------------------------------------------------------------------------

/// With AtLeastOnce (default), the checkpoint is saved AFTER a successful
/// project(). When project() fails, the checkpoint is not updated — the
/// same event will be re-processed if the projector is restarted.
#[tokio::test]
async fn at_least_once_checkpoint_not_saved_when_project_fails() {
    // Given
    let system = ProcessSystem::new().await;
    let event_store = Arc::new(InMemoryEventStore::default());
    let checkpoint_store = Arc::new(InMemoryCheckpointStore::default());
    let agg_id = AggregateId::new("dm-alo-fail-agg");
    let projection_id = ProjectionId::new("dm-alo-fail");

    append_evt(&event_store, &agg_id, 1).await;

    let should_fail = Arc::new(AtomicBool::new(true)); // always fail
    let count = Arc::new(AtomicUsize::new(0));
    let notify = Arc::new(Notify::new());
    let sf_c = Arc::clone(&should_fail);
    let count_c = Arc::clone(&count);
    let notify_c = Arc::clone(&notify);

    // When: spawn with AtLeastOnce (default)
    let _proxy = ProjectorProps::new(
        projection_id.clone(),
        Arc::clone(&event_store) as Arc<dyn EventStore>,
        Arc::clone(&checkpoint_store),
        move || ConditionallyFailingProjector {
            should_fail: Arc::clone(&sf_c),
            count: Arc::clone(&count_c),
            notify: Arc::clone(&notify_c),
        },
    )
    .with_event::<Evt>(Arc::new(UnitCodec))
    .delivery_mode(DeliveryMode::AtLeastOnce)
    .catchup_from_aggregate(agg_id)
    .spawn(&system)
    .await;

    wait_for_count(&count, &notify, 1).await;

    // Give the process time to attempt (and skip) checkpoint save after the failure
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Then: checkpoint is still None — failure prevented the save
    let checkpoint = checkpoint_store
        .load(&projection_id)
        .await
        .expect("load must succeed");
    assert_eq!(
        checkpoint, None,
        "AtLeastOnce must NOT save the checkpoint when project() fails"
    );
}

// ---------------------------------------------------------------------------
// Test: AtLeastOnce — checkpoint is saved after successful project()
// ---------------------------------------------------------------------------

/// With AtLeastOnce, a successful project() results in the checkpoint being
/// saved. This guarantees the event is not re-processed on the next startup
/// (idempotency of the projection is assumed).
#[tokio::test]
async fn at_least_once_checkpoint_saved_after_successful_project() {
    // Given
    let system = ProcessSystem::new().await;
    let event_store = Arc::new(InMemoryEventStore::default());
    let checkpoint_store = Arc::new(InMemoryCheckpointStore::default());
    let agg_id = AggregateId::new("dm-alo-ok-agg");
    let projection_id = ProjectionId::new("dm-alo-ok");

    for seq in 1..=3 {
        append_evt(&event_store, &agg_id, seq).await;
    }

    let should_fail = Arc::new(AtomicBool::new(false)); // always succeed
    let count = Arc::new(AtomicUsize::new(0));
    let notify = Arc::new(Notify::new());
    let sf_c = Arc::clone(&should_fail);
    let count_c = Arc::clone(&count);
    let notify_c = Arc::clone(&notify);

    // When: spawn with AtLeastOnce (default)
    let _proxy = ProjectorProps::new(
        projection_id.clone(),
        Arc::clone(&event_store) as Arc<dyn EventStore>,
        Arc::clone(&checkpoint_store),
        move || ConditionallyFailingProjector {
            should_fail: Arc::clone(&sf_c),
            count: Arc::clone(&count_c),
            notify: Arc::clone(&notify_c),
        },
    )
    .with_event::<Evt>(Arc::new(UnitCodec))
    .delivery_mode(DeliveryMode::AtLeastOnce)
    .catchup_from_aggregate(agg_id)
    .spawn(&system)
    .await;

    // Then: checkpoint eventually reaches 3 (after all 3 successful project() calls)
    wait_for_checkpoint(&checkpoint_store, &projection_id, Some(3)).await;
}

// ---------------------------------------------------------------------------
// Test: AtLeastOnce — re-processing occurs after restart when project failed
// ---------------------------------------------------------------------------

/// Verify the at-least-once guarantee end-to-end:
/// 1. Projector fails on event at seq=2.
/// 2. Checkpoint remains at seq=1 (seq=2 was not saved).
/// 3. On restart (re-spawn with same stores), event at seq=2 is re-processed.
///
/// Uses multi_thread to allow the test task and the process task to run
/// concurrently, so `should_fail` can be set between events.
#[tokio::test(flavor = "multi_thread")]
async fn at_least_once_failed_event_is_reprocessed_after_restart() {
    // Given
    let system = ProcessSystem::new().await;
    let event_store = Arc::new(InMemoryEventStore::default());
    let checkpoint_store = Arc::new(InMemoryCheckpointStore::default());
    let agg_id = AggregateId::new("dm-alo-restart-agg");
    let projection_id = ProjectionId::new("dm-alo-restart");

    // Two events: seq=1 (succeeds), seq=2 (fails on first run)
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
        let projection_id_c = projection_id.clone();
        let event_store_c: Arc<dyn EventStore> = Arc::clone(&event_store) as Arc<dyn EventStore>;
        let checkpoint_store_c = Arc::clone(&checkpoint_store);
        let agg_id_c = agg_id.clone();

        let _proxy = ProjectorProps::new(
            projection_id_c,
            event_store_c,
            checkpoint_store_c,
            move || {
                // Configure to fail after the first project() call (seq=2)
                let inner_sf = Arc::clone(&sf_c);
                let inner_count = Arc::clone(&count_c);
                let inner_notify = Arc::clone(&notify_c);
                ConditionallyFailingProjector {
                    // We use a wrapper that fails starting from the 2nd call.
                    // Since ConditionallyFailingProjector checks should_fail each time,
                    // we set it to false initially and flip it after the first successful call.
                    should_fail: Arc::clone(&inner_sf),
                    count: Arc::clone(&inner_count),
                    notify: Arc::clone(&inner_notify),
                }
            },
        )
        .with_event::<Evt>(Arc::new(UnitCodec))
        .delivery_mode(DeliveryMode::AtLeastOnce)
        .catchup_from_aggregate(agg_id_c)
        .spawn(&system)
        .await;

        // Wait for seq=1 to be processed (count=1), then set should_fail before seq=2
        wait_for_count(&count, &notify, 1).await;
        should_fail.store(true, Ordering::SeqCst);

        // Wait for seq=2 attempt (count=2, failure)
        wait_for_count(&count, &notify, 2).await;

        // Give time for checkpoint save to be skipped
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // Verify: checkpoint is at seq=1 (seq=2 was not saved due to failure)
    let checkpoint_after_first_run = checkpoint_store
        .load(&projection_id)
        .await
        .expect("load must succeed");
    assert_eq!(
        checkpoint_after_first_run,
        Some(1),
        "after first run: checkpoint must be at seq=1 (seq=2 project failed)"
    );

    // Second run: disable failure so seq=2 succeeds
    should_fail.store(false, Ordering::SeqCst);
    count.store(0, Ordering::SeqCst); // reset counter for second run

    let count2 = Arc::clone(&count);
    let notify2 = Arc::clone(&notify);
    let sf2 = Arc::clone(&should_fail);

    let _proxy2 = ProjectorProps::new(
        projection_id.clone(),
        Arc::clone(&event_store) as Arc<dyn EventStore>,
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

    // seq=2 is re-processed (restart from checkpoint seq=1 → process seq=2)
    wait_for_count(&count, &notify, 1).await;

    // Then: checkpoint now at seq=2
    wait_for_checkpoint(&checkpoint_store, &projection_id, Some(2)).await;
}

// ---------------------------------------------------------------------------
// Test: ExactlyOnce — framework does NOT save checkpoint
// ---------------------------------------------------------------------------

/// With ExactlyOnce, the framework never saves the checkpoint — even on
/// successful project(). The user is expected to save both the read-model
/// update and the checkpoint atomically inside project() via their own TX.
///
/// This test verifies the negative: after a successful projection run with
/// ExactlyOnce, the framework-managed checkpoint is still None.
#[tokio::test]
async fn exactly_once_framework_does_not_save_checkpoint() {
    // Given
    let system = ProcessSystem::new().await;
    let event_store = Arc::new(InMemoryEventStore::default());
    let checkpoint_store = Arc::new(InMemoryCheckpointStore::default());
    let agg_id = AggregateId::new("dm-eo-agg");
    let projection_id = ProjectionId::new("dm-eo");

    append_evt(&event_store, &agg_id, 1).await;

    let should_fail = Arc::new(AtomicBool::new(false));
    let count = Arc::new(AtomicUsize::new(0));
    let notify = Arc::new(Notify::new());
    let sf_c = Arc::clone(&should_fail);
    let count_c = Arc::clone(&count);
    let notify_c = Arc::clone(&notify);

    // When: spawn with ExactlyOnce (framework skips checkpoint saving)
    let _proxy = ProjectorProps::new(
        projection_id.clone(),
        Arc::clone(&event_store) as Arc<dyn EventStore>,
        Arc::clone(&checkpoint_store),
        move || ConditionallyFailingProjector {
            should_fail: Arc::clone(&sf_c),
            count: Arc::clone(&count_c),
            notify: Arc::clone(&notify_c),
        },
    )
    .with_event::<Evt>(Arc::new(UnitCodec))
    .delivery_mode(DeliveryMode::ExactlyOnce)
    .catchup_from_aggregate(agg_id)
    .spawn(&system)
    .await;

    wait_for_count(&count, &notify, 1).await;

    // Give the process time to complete checkpoint-save (which it should NOT do)
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Then: checkpoint is still None — framework does not save it for ExactlyOnce
    let checkpoint = checkpoint_store
        .load(&projection_id)
        .await
        .expect("load must succeed");
    assert_eq!(
        checkpoint, None,
        "ExactlyOnce must NOT save the checkpoint; the user controls it inside project()"
    );

    assert_eq!(
        count.load(Ordering::SeqCst),
        1,
        "project() must still be called once; ExactlyOnce only skips the framework checkpoint save"
    );
}
