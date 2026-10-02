//! The resident scheduler is a cache, not a record.
//!
//! Where a deadline durably lives is each saga's own stream: the `scheduled`
//! marker is written in the same atomic batch as the decision that set it.  The
//! scheduler's in-memory structure only accelerates those facts, so starting one
//! after a restart must restore nothing — re-arming is each instance's own
//! business, and happens when the instance is activated
//! (`saga_deadline_scheduler.rs::replay_reregisters_unfired_schedule_and_fires_it`
//! is that positive half).
//!
//! What this file rules out is the tempting shortcut: a scheduler that reads
//! streams of its own at start-up to rebuild the deadlines it lost.  The store
//! holding the unfired marker is wired in as the system's default event store,
//! so a scheduler that went looking has somewhere to find it; the stream is then
//! read back row by row and must still hold nothing but the one seeded marker.
//!
//! Run with:
//! `cargo test -p nitinol-saga --test saga_scheduler_has_no_recovery_step`.

#[path = "common/helpers.rs"]
mod common;
use common::{
    encode_schedule_scheduled, resident_scheduler, schedule_kind_of, schedule_transition,
    SCHEDULE_MARKER,
};

use std::sync::Arc;
use std::time::Duration;

use futures_util::TryStreamExt;

use nitinol_eventsource::system::EventSourceSystem;
use nitinol_persistence::store::{EventStore, InMemoryEventStore};
use nitinol_persistence::{AppendingEvent, LoadQuery, LoadedEvent};
use nitinol_runtime::ProcessSystem;
use nitinol_saga::{SagaId, TimerName};

/// The saga the seeded marker belongs to.  No process for it is ever started
/// here — its absence is the point.
const ORPHANED_SAGA_ID: &str = "sched-no-recovery-saga";

/// Short, so the wait below is cheap: the marker is already past its deadline
/// long before the assertion runs.
const DEADLINE: Duration = Duration::from_millis(200);

/// What the scheduled payload would have been.  Opaque bytes to the scheduler,
/// which is exactly why a scheduler cannot restore a deadline on its own: there
/// is nobody to decode it for.
const SCHEDULED_PAYLOAD: &[u8] = br#"{"note":"never-fires"}"#;

async fn load_stream(store: &Arc<dyn EventStore>, saga_id: &SagaId) -> Vec<LoadedEvent> {
    store
        .load(LoadQuery::by_stream(saga_id))
        .await
        .expect("load saga stream must succeed")
        .try_collect()
        .await
        .expect("collect saga events must succeed")
}

/// Given a store holding an unfired `scheduled` marker and nothing else,
/// when a resident scheduler is started over that store as the system default
/// and the deadline passes,
/// then the stream is untouched — no firing, and no bookkeeping of the
/// scheduler's own.
#[tokio::test]
async fn a_resident_scheduler_alone_restores_nothing() {
    // Given: the durable trace of a deadline a crashed incarnation had set
    let saga_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let saga_id = SagaId::new(ORPHANED_SAGA_ID);
    saga_store
        .append(
            saga_id.as_str(),
            vec![AppendingEvent {
                sequence: 1,
                event_type: SCHEDULE_MARKER,
                payload: encode_schedule_scheduled(
                    &saga_id,
                    &TimerName::new("reminder"),
                    DEADLINE,
                    SCHEDULED_PAYLOAD,
                    jiff::Timestamp::now().as_millisecond(),
                ),
                occurred_at: jiff::Timestamp::now(),
            }],
        )
        .await
        .expect("seeding the unfired scheduled marker must succeed");

    // When: only the scheduler comes back, with that store as the default one
    let ps = ProcessSystem::new().await;
    let system = EventSourceSystem::builder(ps)
        .with_event_store(Arc::clone(&saga_store))
        .build();
    let _scheduler = resident_scheduler(system.process_system()).await;

    // The deadline is reached several times over: a scheduler that had restored
    // it would have fired it by now.
    tokio::time::sleep(DEADLINE * 5).await;

    // Then: every row on the stream is still the one that was seeded
    let events = load_stream(&saga_store, &saga_id).await;
    let transitions: Vec<&'static str> = events
        .iter()
        .filter_map(schedule_kind_of)
        .map(|kind| schedule_transition(&kind))
        .collect();
    assert_eq!(
        transitions,
        vec!["scheduled"],
        "starting the resident scheduler must not re-arm or fire a deadline it \
         never held: re-arming belongs to the instance's own activation"
    );
    assert_eq!(
        events.len(),
        1,
        "the scheduler must write nothing of its own to a saga's stream; rows={:?}",
        events
            .iter()
            .map(|event| event.event_type.to_string())
            .collect::<Vec<_>>()
    );
}
