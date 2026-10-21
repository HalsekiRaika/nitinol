// E2E test: Offset delivery modes for ProjectorProcess.
//
// Scenario: ProjectorProcess + OffsetStore, three delivery modes.
//
// Three E2E stories:
//   1. AtLeastOnce  — failed projection is retried on restart because the
//      offset was NOT saved (the event is reprocessed after restart).
//   2. ExactlyOnce  — the user saves both the read-model update AND the
//      offset inside project(). On restart, no events are reprocessed.
//   3. AtMostOnce   — the offset is saved BEFORE project() so a failed
//      projection is treated as "delivered" (no reprocessing on restart).

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use tokio::sync::Notify;

use nitinol_eventsource::codec::Codec;
use nitinol_eventsource::system::EventSourceSystem;
use nitinol_eventsource::{Event, ProjectionContext, ProjectionId, Projector, ProjectorProps};
use nitinol_persistence::provider::{CustomStore, PersistenceProvider};
use nitinol_persistence::store::{
    DeliveryMode, EventStore, InMemoryEventStore, InMemoryOffsetStore, OffsetScope, OffsetStore,
};
use nitinol_persistence::{AggregateId, AppendingEvent, EventType, Family, TypeName};
use nitinol_runtime::ProcessSystem;

// Fixtures: event

#[derive(Clone)]
struct Evt;

impl Event for Evt {
    const EVENT_TYPE: EventType = EventType::new(Family::new("e2e.ckpt"), TypeName::new("Evt"));

    type Codec = UnitCodec;
}

// Fixtures: pass-through codec

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

// Fixtures: ExactlyOnceProjector — user saves offset inside project()

struct ExactlyOnceProjector {
    /// Direct reference to the offset store so the user can save atomically.
    offset_store: InMemoryOffsetStore,
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
        // Simulate an atomic "read-model update + offset save" in a single TX.
        // For InMemory stores this is sequential, not truly atomic, but demonstrates
        // the ExactlyOnce user pattern.
        self.offset_store
            .save(
                &OffsetScope::from(&self.projection_id),
                ctx.current_sequence(),
                None,
            )
            .await
            .expect("user offset save must succeed");
        self.count.fetch_add(1, Ordering::SeqCst);
        self.notify.notify_one();
        Ok(())
    }
}

// Helpers

/// Append one Evt to the event store at the given sequence number.
async fn append_evt(store: &InMemoryEventStore, agg_id: &AggregateId, sequence: u64) {
    store
        .append(
            agg_id.as_str(),
            vec![AppendingEvent {
                sequence,
                event_type: EventType::new(Family::new("e2e.ckpt"), TypeName::new("Evt")),
                payload: Bytes::new(),
                occurred_at: jiff::Timestamp::now(),
            }],
        )
        .await
        .expect("append must succeed");
}

/// Poll the offset store until it reaches `expected` or until the timeout elapses.
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

// Test 1: AtLeastOnce — failed event retried on restart

/// Given two events (seq=1, seq=2) and an AtLeastOnce projector that fails on seq=2,
/// When the projector is restarted,
/// Then seq=2 is reprocessed (at-least-once guarantee) and the offset reaches 2.
///
/// Steps:
///   First run:  seq=1 → ok (offset=1), seq=2 → fail (offset stays=1)
///   Restart:    offset=1 → start from seq=2 → succeeds → offset=2
#[tokio::test(flavor = "multi_thread")]
async fn e2e_at_least_once_failed_projection_retried_on_restart() {
    // Given
    let event_store = InMemoryEventStore::default();
    let offset_store = InMemoryOffsetStore::default();
    let system = EventSourceSystem::builder(ProcessSystem::new().await)
        .with_persistence(PersistenceProvider::new(
            CustomStore::builder()
                .eventstore(event_store.clone())
                .projection_offsets(offset_store.clone())
                .build(),
        ))
        .build();
    let agg_id = AggregateId::new("e2e-ckpt-alo-agg");
    let projection_id = ProjectionId::new("e2e-ckpt-alo");

    // Only append seq=1 initially; seq=2 is appended after the failure flag is set
    // to eliminate the race where the projector processes seq=2 before the flag is tripped.
    append_evt(&event_store, &agg_id, 1).await;

    let should_fail = Arc::new(AtomicBool::new(false));
    let count = Arc::new(AtomicUsize::new(0));
    let notify = Arc::new(Notify::new());

    // First run: succeed on seq=1, fail on seq=2
    let proxy = {
        let sf_c = Arc::clone(&should_fail);
        let count_c = Arc::clone(&count);
        let notify_c = Arc::clone(&notify);

        ProjectorProps::new(projection_id.clone(), move || {
            ConditionallyFailingProjector {
                should_fail: Arc::clone(&sf_c),
                count: Arc::clone(&count_c),
                notify: Arc::clone(&notify_c),
            }
        })
        .with_event::<Evt>()
        .delivery_mode(DeliveryMode::AtLeastOnce)
        .catchup_from_aggregate(agg_id.clone())
        .spawn(&system)
        .await
    };

    // Wait for seq=1 to succeed, set the failure flag, then append seq=2.
    // This ordering ensures the flag is already true when the projector sees seq=2.
    wait_for_count(&count, &notify, 1).await;
    should_fail.store(true, Ordering::SeqCst);
    append_evt(&event_store, &agg_id, 2).await;

    // Wait for the seq=2 failure attempt
    wait_for_count(&count, &notify, 2).await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Explicitly stop the first projector and wait for it to leave the registry
    // before starting the second — ensures a true restart rather than two concurrent projectors.
    let proxy_pid = proxy.pid();
    proxy.stop().await.expect("first projector stop");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if system.process_system().lookup(proxy_pid).await.is_none() {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "first projector must unregister within 5s"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // Verify offset is at seq=1 (seq=2 was not saved due to failure)
    let ckpt_after_first = offset_store
        .load(&OffsetScope::from(&projection_id))
        .await
        .expect("load must succeed");
    assert_eq!(
        ckpt_after_first,
        Some(1),
        "after first run: offset must be seq=1 (seq=2 failed)"
    );

    // When: restart with failure disabled
    should_fail.store(false, Ordering::SeqCst);
    count.store(0, Ordering::SeqCst);

    // Clone before the move closure so the originals are available for assertions.
    let sf2 = Arc::clone(&should_fail);
    let count2 = Arc::clone(&count);
    let notify2 = Arc::clone(&notify);

    let _proxy2 = ProjectorProps::new(projection_id.clone(), move || {
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

    // seq=2 is reprocessed on restart
    wait_for_count(&count, &notify, 1).await;

    // Then: offset advances to seq=2 after successful reprocessing
    wait_for_offset(&offset_store, &projection_id, Some(2)).await;
}

// Test 2: ExactlyOnce — user saves offset inside project(); no reprocessing on restart

/// Given two events and an ExactlyOnce projector that saves its own offset
/// inside project() (simulating an atomic read-model + offset TX),
/// When the projector is restarted,
/// Then no events are reprocessed (offset=2 → start from seq=3 → 0 events).
#[tokio::test]
async fn e2e_exactly_once_user_saves_offset_prevents_reprocessing() {
    // Given
    let event_store = InMemoryEventStore::default();
    let offset_store = InMemoryOffsetStore::default();
    let system = EventSourceSystem::builder(ProcessSystem::new().await)
        .with_persistence(PersistenceProvider::new(
            CustomStore::builder()
                .eventstore(event_store.clone())
                .projection_offsets(offset_store.clone())
                .build(),
        ))
        .build();
    let agg_id = AggregateId::new("e2e-ckpt-eo-agg");
    let projection_id = ProjectionId::new("e2e-ckpt-eo");

    append_evt(&event_store, &agg_id, 1).await;
    append_evt(&event_store, &agg_id, 2).await;

    let count = Arc::new(AtomicUsize::new(0));
    let notify = Arc::new(Notify::new());

    // First run: process both events, user saves offset inside project()
    {
        let count_c = Arc::clone(&count);
        let notify_c = Arc::clone(&notify);
        let ckpt_c = offset_store.clone();
        let proj_id_c = projection_id.clone();

        let _proxy = ProjectorProps::new(projection_id.clone(), move || ExactlyOnceProjector {
            offset_store: ckpt_c.clone(),
            projection_id: proj_id_c.clone(),
            count: Arc::clone(&count_c),
            notify: Arc::clone(&notify_c),
        })
        .with_event::<Evt>()
        .delivery_mode(DeliveryMode::ExactlyOnce)
        .catchup_from_aggregate(agg_id.clone())
        .spawn(&system)
        .await;

        wait_for_count(&count, &notify, 2).await;
    }

    // Offset was saved by the user inside project()
    let ckpt_after_first = offset_store
        .load(&OffsetScope::from(&projection_id))
        .await
        .expect("load must succeed");
    assert_eq!(
        ckpt_after_first,
        Some(2),
        "user must have saved offset=2 inside project()"
    );

    // When: restart the projector — offset=2, no events after seq=2
    count.store(0, Ordering::SeqCst);

    let count_c2 = Arc::clone(&count);
    let notify_c2 = Arc::clone(&notify);
    let ckpt_c2 = offset_store.clone();
    let proj_id_c2 = projection_id.clone();

    let _proxy2 = ProjectorProps::new(projection_id.clone(), move || ExactlyOnceProjector {
        offset_store: ckpt_c2.clone(),
        projection_id: proj_id_c2.clone(),
        count: Arc::clone(&count_c2),
        notify: Arc::clone(&notify_c2),
    })
    .with_event::<Evt>()
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
        "on restart with offset=2, no events must be reprocessed (exactly-once)"
    );
}

// Test 3: AtMostOnce — failed event is NOT retried on restart

/// Given one event (seq=1) and an AtMostOnce projector whose project() fails,
/// When the projector is restarted,
/// Then no events are reprocessed (offset was saved BEFORE the failing project() call).
#[tokio::test]
async fn e2e_at_most_once_failed_event_not_retried_on_restart() {
    // Given
    let event_store = InMemoryEventStore::default();
    let offset_store = InMemoryOffsetStore::default();
    let system = EventSourceSystem::builder(ProcessSystem::new().await)
        .with_persistence(PersistenceProvider::new(
            CustomStore::builder()
                .eventstore(event_store.clone())
                .projection_offsets(offset_store.clone())
                .build(),
        ))
        .build();
    let agg_id = AggregateId::new("e2e-ckpt-amo-agg");
    let projection_id = ProjectionId::new("e2e-ckpt-amo");

    append_evt(&event_store, &agg_id, 1).await;

    let should_fail = Arc::new(AtomicBool::new(true)); // always fail
    let count = Arc::new(AtomicUsize::new(0));
    let notify = Arc::new(Notify::new());

    // First run: AtMostOnce saves offset BEFORE calling project(), then project() fails
    {
        let sf_c = Arc::clone(&should_fail);
        let count_c = Arc::clone(&count);
        let notify_c = Arc::clone(&notify);

        let _proxy = ProjectorProps::new(projection_id.clone(), move || {
            ConditionallyFailingProjector {
                should_fail: Arc::clone(&sf_c),
                count: Arc::clone(&count_c),
                notify: Arc::clone(&notify_c),
            }
        })
        .with_event::<Evt>()
        .delivery_mode(DeliveryMode::AtMostOnce)
        .catchup_from_aggregate(agg_id.clone())
        .spawn(&system)
        .await;

        wait_for_count(&count, &notify, 1).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // Offset IS at seq=1 (saved before project() was called — AtMostOnce)
    let ckpt_after_first = offset_store
        .load(&OffsetScope::from(&projection_id))
        .await
        .expect("load must succeed");
    assert_eq!(
        ckpt_after_first,
        Some(1),
        "AtMostOnce: offset must be seq=1 even though project() failed"
    );

    // When: restart the projector — offset=1, no events after seq=1
    should_fail.store(false, Ordering::SeqCst);
    count.store(0, Ordering::SeqCst);

    // Clone before the move closure so the originals remain accessible for assertions.
    let sf2 = Arc::clone(&should_fail);
    let count2 = Arc::clone(&count);
    let notify2 = Arc::clone(&notify);

    let _proxy2 = ProjectorProps::new(projection_id, move || ConditionallyFailingProjector {
        should_fail: Arc::clone(&sf2),
        count: Arc::clone(&count2),
        notify: Arc::clone(&notify2),
    })
    .with_event::<Evt>()
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
        "AtMostOnce: failed event must NOT be retried on restart (offset was already advanced)"
    );
}
