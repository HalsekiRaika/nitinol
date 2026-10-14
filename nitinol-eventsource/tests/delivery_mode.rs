// Integration tests for DeliveryMode (AtMostOnce / AtLeastOnce / ExactlyOnce).
//
// Tests verify the offset-save ordering relative to project() for each mode,
// and the exact semantics when project() fails.
//
// AtMostOnce:
//   offset is saved BEFORE project(). If project() fails, the offset
//   is already advanced — the event will NOT be re-processed on restart.
//
// AtLeastOnce (default):
//   offset is saved AFTER a successful project(). If project() fails, the
//   offset is NOT advanced — the event WILL be re-processed on restart.
//
// ExactlyOnce:
//   the framework does NOT save the offset at all. The user is responsible
//   for saving both the read-model update and the offset inside project()
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

use nitinol_eventsource::{
    codec::Codec, Event, ProjectionContext, ProjectionId, Projector, ProjectorProps,
};
use nitinol_persistence::provider::{CustomStore, PersistenceProvider};
use nitinol_persistence::store::{
    DeliveryMode, EventStore, InMemoryEventStore, InMemoryOffsetStore, OffsetScope, OffsetStore,
};
use nitinol_persistence::{AggregateId, AppendingEvent, EventType, Family, TypeName};
use nitinol_runtime::ProcessSystem;

// Fixtures: event type

#[derive(Clone)]
struct Evt;

impl Event for Evt {
    const EVENT_TYPE: EventType = EventType::new(Family::new(""), TypeName::new("Evt"));

    type Codec = UnitCodec;
}

// Fixtures: codec

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

// Fixtures: ConditionallyFailingProjector
//
// When `should_fail` is true, project() returns an error.
// When false, project() succeeds.
// The call count and notify are shared so tests can observe execution.

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

// Helpers

async fn append_evt(store: &InMemoryEventStore, agg_id: &AggregateId, sequence: u64) {
    store
        .append(
            agg_id.as_str(),
            vec![AppendingEvent {
                sequence,
                event_type: EventType::new(Family::new(""), TypeName::new("Evt")),
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

/// Poll the offset store until it reaches the expected value or timeout.
async fn wait_for_offset(
    store: &InMemoryOffsetStore,
    projection_id: &ProjectionId,
    expected: Option<u64>,
) {
    tokio::time::timeout(Duration::from_millis(500), async {
        loop {
            let current = store
                .load(&OffsetScope::from(projection_id))
                .await
                .expect("offset load must succeed");
            if current == expected {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for offset to reach {:?}", expected));
}

// Test: AtMostOnce — offset is advanced even when project() fails

/// With AtMostOnce, the offset is saved BEFORE calling project().
/// If project() fails, the offset has already advanced — the event is
/// treated as "processed" (at most once delivery) even though it was not
/// actually processed successfully.
#[tokio::test]
async fn at_most_once_offset_advanced_even_when_project_fails() {
    // Given
    let system = ProcessSystem::new().await;
    let event_store = InMemoryEventStore::default();
    let offset_store = InMemoryOffsetStore::default();
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
    let persistence = PersistenceProvider::new(
        CustomStore::builder()
            .eventstore(event_store.clone())
            .projection_offsets(offset_store.clone())
            .build(),
    );
    let _proxy = ProjectorProps::new(projection_id.clone(), &persistence, move || {
        ConditionallyFailingProjector {
            should_fail: Arc::clone(&sf_c),
            count: Arc::clone(&count_c),
            notify: Arc::clone(&notify_c),
        }
    })
    .with_event::<Evt>()
    .delivery_mode(DeliveryMode::AtMostOnce)
    .catchup_from_aggregate(agg_id)
    .spawn(&system)
    .await;

    // Wait for project() to be called
    wait_for_count(&count, &notify, 1).await;

    // AtMostOnce saves the offset BEFORE calling project(). When project() fires the
    // Notify, the offset is already saved.
    // Then: offset is at seq=1 despite project() failing
    wait_for_offset(&offset_store, &projection_id, Some(1)).await;

    assert_eq!(
        count.load(Ordering::SeqCst),
        1,
        "project() must have been called once"
    );
}

// Test: AtMostOnce — offset advances on success too

/// With AtMostOnce, a successful project() also results in the offset
/// being saved (it was saved before the call).
#[tokio::test]
async fn at_most_once_offset_advanced_on_success() {
    // Given
    let system = ProcessSystem::new().await;
    let event_store = InMemoryEventStore::default();
    let offset_store = InMemoryOffsetStore::default();
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
    let persistence = PersistenceProvider::new(
        CustomStore::builder()
            .eventstore(event_store.clone())
            .projection_offsets(offset_store.clone())
            .build(),
    );
    let _proxy = ProjectorProps::new(projection_id.clone(), &persistence, move || {
        ConditionallyFailingProjector {
            should_fail: Arc::clone(&sf_c),
            count: Arc::clone(&count_c),
            notify: Arc::clone(&notify_c),
        }
    })
    .with_event::<Evt>()
    .delivery_mode(DeliveryMode::AtMostOnce)
    .catchup_from_aggregate(agg_id)
    .spawn(&system)
    .await;

    // Wait for all 3 project() calls, then for offset to settle at 3
    wait_for_count(&count, &notify, 3).await;

    // Then: offset reaches 3
    wait_for_offset(&offset_store, &projection_id, Some(3)).await;
}

// Test: AtLeastOnce — offset NOT saved when project() fails

/// With AtLeastOnce (default), the offset is saved AFTER a successful
/// project(). When project() fails, the offset is not updated — the
/// same event will be re-processed if the projector is restarted.
#[tokio::test]
async fn at_least_once_offset_not_saved_when_project_fails() {
    // Given
    let system = ProcessSystem::new().await;
    let event_store = InMemoryEventStore::default();
    let offset_store = InMemoryOffsetStore::default();
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
    let persistence = PersistenceProvider::new(
        CustomStore::builder()
            .eventstore(event_store.clone())
            .projection_offsets(offset_store.clone())
            .build(),
    );
    let _proxy = ProjectorProps::new(projection_id.clone(), &persistence, move || {
        ConditionallyFailingProjector {
            should_fail: Arc::clone(&sf_c),
            count: Arc::clone(&count_c),
            notify: Arc::clone(&notify_c),
        }
    })
    .with_event::<Evt>()
    .delivery_mode(DeliveryMode::AtLeastOnce)
    .catchup_from_aggregate(agg_id)
    .spawn(&system)
    .await;

    wait_for_count(&count, &notify, 1).await;

    // Give the process time to attempt (and skip) offset save after the failure
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Then: offset is still None — failure prevented the save
    let offset = offset_store
        .load(&OffsetScope::from(&projection_id))
        .await
        .expect("load must succeed");
    assert_eq!(
        offset, None,
        "AtLeastOnce must NOT save the offset when project() fails"
    );
}

// Test: AtLeastOnce — offset is saved after successful project()

/// With AtLeastOnce, a successful project() results in the offset being
/// saved. This guarantees the event is not re-processed on the next startup
/// (idempotency of the projection is assumed).
#[tokio::test]
async fn at_least_once_offset_saved_after_successful_project() {
    // Given
    let system = ProcessSystem::new().await;
    let event_store = InMemoryEventStore::default();
    let offset_store = InMemoryOffsetStore::default();
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
    let persistence = PersistenceProvider::new(
        CustomStore::builder()
            .eventstore(event_store.clone())
            .projection_offsets(offset_store.clone())
            .build(),
    );
    let _proxy = ProjectorProps::new(projection_id.clone(), &persistence, move || {
        ConditionallyFailingProjector {
            should_fail: Arc::clone(&sf_c),
            count: Arc::clone(&count_c),
            notify: Arc::clone(&notify_c),
        }
    })
    .with_event::<Evt>()
    .delivery_mode(DeliveryMode::AtLeastOnce)
    .catchup_from_aggregate(agg_id)
    .spawn(&system)
    .await;

    // Then: offset eventually reaches 3 (after all 3 successful project() calls)
    wait_for_offset(&offset_store, &projection_id, Some(3)).await;
}

// Test: AtLeastOnce — re-processing occurs after restart when project failed

/// Verify the at-least-once guarantee end-to-end:
/// 1. Projector fails on event at seq=2.
/// 2. Offset remains at seq=1 (seq=2 was not saved).
/// 3. On restart (re-spawn with same stores), event at seq=2 is re-processed.
///
/// Uses multi_thread to allow the test task and the process task to run
/// concurrently, so `should_fail` can be set between events.
#[tokio::test(flavor = "multi_thread")]
async fn at_least_once_failed_event_is_reprocessed_after_restart() {
    // Given
    let system = ProcessSystem::new().await;
    let event_store = InMemoryEventStore::default();
    let offset_store = InMemoryOffsetStore::default();
    let agg_id = AggregateId::new("dm-alo-restart-agg");
    let projection_id = ProjectionId::new("dm-alo-restart");

    // seq=1 is appended before spawn; seq=2 is deferred until after should_fail is
    // set so the projector cannot process seq=2 before the failure flag is armed.
    append_evt(&event_store, &agg_id, 1).await;

    let should_fail = Arc::new(AtomicBool::new(false));
    let count = Arc::new(AtomicUsize::new(0));
    let notify = Arc::new(Notify::new());

    // First run: succeed on seq=1, fail on seq=2
    let proxy = {
        let sf_c = Arc::clone(&should_fail);
        let count_c = Arc::clone(&count);
        let notify_c = Arc::clone(&notify);
        let projection_id_c = projection_id.clone();
        let event_store_c = event_store.clone();
        let offset_store_c = offset_store.clone();
        let agg_id_c = agg_id.clone();

        let persistence = PersistenceProvider::new(
            CustomStore::builder()
                .eventstore(event_store_c)
                .projection_offsets(offset_store_c)
                .build(),
        );
        ProjectorProps::new(projection_id_c, &persistence, move || {
            ConditionallyFailingProjector {
                should_fail: Arc::clone(&sf_c),
                count: Arc::clone(&count_c),
                notify: Arc::clone(&notify_c),
            }
        })
        .with_event::<Evt>()
        .delivery_mode(DeliveryMode::AtLeastOnce)
        .catchup_from_aggregate(agg_id_c)
        .spawn(&system)
        .await
    };

    // Wait for seq=1 to be processed, then arm the failure flag and add seq=2.
    // Appending seq=2 only after the flag is set eliminates the race where the
    // projector processes seq=2 before should_fail becomes true.
    wait_for_count(&count, &notify, 1).await;
    should_fail.store(true, Ordering::SeqCst);
    append_evt(&event_store, &agg_id, 2).await;

    // Wait for seq=2 to be attempted and fail.
    wait_for_count(&count, &notify, 2).await;

    // Stop the first projector and wait for it to fully deregister before
    // starting the second run; without this, both projectors can race on
    // the offset store.
    let proxy_pid = proxy.pid();
    let _ = proxy.stop().await;
    {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if system.lookup(proxy_pid).await.is_none() {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "timed out waiting for projector to fully stop"
            );
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    }

    // Verify: offset is at seq=1 (seq=2 was not saved due to failure)
    let offset_after_first_run = offset_store
        .load(&OffsetScope::from(&projection_id))
        .await
        .expect("load must succeed");
    assert_eq!(
        offset_after_first_run,
        Some(1),
        "after first run: offset must be at seq=1 (seq=2 project failed)"
    );

    // Second run: disable failure so seq=2 succeeds
    should_fail.store(false, Ordering::SeqCst);
    count.store(0, Ordering::SeqCst); // reset counter for second run

    let count2 = Arc::clone(&count);
    let notify2 = Arc::clone(&notify);
    let sf2 = Arc::clone(&should_fail);

    let persistence = PersistenceProvider::new(
        CustomStore::builder()
            .eventstore(event_store.clone())
            .projection_offsets(offset_store.clone())
            .build(),
    );
    let _proxy2 = ProjectorProps::new(projection_id.clone(), &persistence, move || {
        ConditionallyFailingProjector {
            should_fail: Arc::clone(&sf2),
            count: Arc::clone(&count2),
            notify: Arc::clone(&notify2),
        }
    })
    .with_event::<Evt>()
    .delivery_mode(DeliveryMode::AtLeastOnce)
    .catchup_from_aggregate(agg_id)
    .spawn(&system)
    .await;

    // seq=2 is re-processed (restart from offset seq=1 → process seq=2)
    wait_for_count(&count, &notify, 1).await;

    // Then: offset now at seq=2
    wait_for_offset(&offset_store, &projection_id, Some(2)).await;
}

// Test: ExactlyOnce — framework does NOT save offset

/// With ExactlyOnce, the framework never saves the offset — even on
/// successful project(). The user is expected to save both the read-model
/// update and the offset atomically inside project() via their own TX.
///
/// This test verifies the negative: after a successful projection run with
/// ExactlyOnce, the framework-managed offset is still None.
#[tokio::test]
async fn exactly_once_framework_does_not_save_offset() {
    // Given
    let system = ProcessSystem::new().await;
    let event_store = InMemoryEventStore::default();
    let offset_store = InMemoryOffsetStore::default();
    let agg_id = AggregateId::new("dm-eo-agg");
    let projection_id = ProjectionId::new("dm-eo");

    append_evt(&event_store, &agg_id, 1).await;

    let should_fail = Arc::new(AtomicBool::new(false));
    let count = Arc::new(AtomicUsize::new(0));
    let notify = Arc::new(Notify::new());
    let sf_c = Arc::clone(&should_fail);
    let count_c = Arc::clone(&count);
    let notify_c = Arc::clone(&notify);

    // When: spawn with ExactlyOnce (framework skips offset saving)
    let persistence = PersistenceProvider::new(
        CustomStore::builder()
            .eventstore(event_store.clone())
            .projection_offsets(offset_store.clone())
            .build(),
    );
    let _proxy = ProjectorProps::new(projection_id.clone(), &persistence, move || {
        ConditionallyFailingProjector {
            should_fail: Arc::clone(&sf_c),
            count: Arc::clone(&count_c),
            notify: Arc::clone(&notify_c),
        }
    })
    .with_event::<Evt>()
    .delivery_mode(DeliveryMode::ExactlyOnce)
    .catchup_from_aggregate(agg_id)
    .spawn(&system)
    .await;

    wait_for_count(&count, &notify, 1).await;

    // Give the process time to complete offset-save (which it should NOT do)
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Then: offset is still None — framework does not save it for ExactlyOnce
    let offset = offset_store
        .load(&OffsetScope::from(&projection_id))
        .await
        .expect("load must succeed");
    assert_eq!(
        offset, None,
        "ExactlyOnce must NOT save the offset; the user controls it inside project()"
    );

    assert_eq!(
        count.load(Ordering::SeqCst),
        1,
        "project() must still be called once; ExactlyOnce only skips the framework offset save"
    );
}
