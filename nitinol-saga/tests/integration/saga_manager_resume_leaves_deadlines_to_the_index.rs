//! A manager that resumes from its offset re-delivers nothing, and a pending
//! deadline still arrives.
//!
//! ```gherkin
//!   Scenario: the deadline index carries what the re-read used to
//!     Given a saga with a pending deadline crosses a restart, and no new
//!           upstream record arrives
//!     When the manager resumed from its offset re-delivers nothing
//!     Then the deadline reaches handle on the index's request
//! ```
//!
//! Before the offset, a restarted manager re-read its source from the floor,
//! and that re-read happened to wake every instance it touched — a pending
//! deadline among them. This test takes that accident away the way production
//! does: the subscription keeps its floor at the beginning, and only the
//! recorded offset keeps the trigger from being read again. What is left to
//! bring the instance back is the schedule fact on its stream and the index
//! that reads it.

use super::deadline_index;
use super::manager_offsets;
use deadline_index::{
    append_trigger, count_schedule_transition, fired, spawn_index_saga, wait_until_fired,
    wait_until_scheduled, ProbeJournal, Recorded, Trigger, PROBE_INTERVAL, PROBE_TIMEOUT,
};
use manager_offsets::manager_scope;

use std::time::Instant;

use nitinol_eventsource::system::EventSourceSystem;
use nitinol_eventsource::Event;
use nitinol_persistence::provider::{CustomStore, PersistenceProvider, Unprovided};
use nitinol_persistence::store::{InMemoryEventStore, InMemoryOffsetStore, OffsetStore};
use nitinol_runtime::ProcessSystem;
use nitinol_saga::{SagaId, Subscription};

const UPSTREAM: &str = "manager-resume-deadline-upstream";
const SAGA: &str = "manager-resume-deadline-1";

/// Long enough that the deadline is still pending when the first system stops.
const AFTER_MILLIS: u64 = 2_000;

/// The trigger's stream sequence, and so the offset the first system records.
const TRIGGER_SEQUENCE: u64 = 1;

type ResumeStore = CustomStore<Unprovided, Unprovided, InMemoryOffsetStore>;

async fn system_over(
    journal: &ProbeJournal,
    offsets: &InMemoryOffsetStore,
) -> EventSourceSystem<ResumeStore> {
    EventSourceSystem::builder(ProcessSystem::new().await)
        .with_persistence(PersistenceProvider::new(
            CustomStore::builder()
                .eventstore(journal.clone())
                .saga_offsets(offsets.clone())
                .build(),
        ))
        .build()
}

/// The scope the index saga's manager records its one source under: the host
/// is unnamed, so its identifier is the path of the saga's own event type.
fn trigger_scope() -> nitinol_persistence::store::OffsetScope {
    manager_scope(
        &Recorded::EVENT_TYPE.to_path().to_string(),
        Trigger::EVENT_TYPE,
        0,
    )
}

async fn wait_until_recorded(offsets: &InMemoryOffsetStore, want: u64) {
    let scope = trigger_scope();
    let deadline = Instant::now() + PROBE_TIMEOUT;
    loop {
        let held = offsets
            .load(&scope)
            .await
            .expect("an in-memory offset load succeeds");
        if held.is_some_and(|held| held >= want) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "the first system's manager must record its offset before it stops; \
             {} holds {held:?}",
            scope.as_str()
        );
        tokio::time::sleep(PROBE_INTERVAL).await;
    }
}

/// Run a system over `inner` and `offsets` on a runtime of its own, hand its
/// manager the trigger, and once both the deadline and the manager's offset are
/// recorded, stop the whole system by dropping its runtime.
fn run_until_recorded_then_stop(inner: &InMemoryEventStore, offsets: &InMemoryOffsetStore) {
    let inner = inner.clone();
    let offsets = offsets.clone();
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("the stopping system's runtime must build");
        runtime.block_on(async {
            let probe = ProbeJournal::recording(inner);
            let system = system_over(&probe, &offsets).await;
            let journal = probe.journal();
            append_trigger(
                &journal,
                UPSTREAM,
                TRIGGER_SEQUENCE,
                &Trigger {
                    saga: SAGA.to_owned(),
                    note: "after-resume".to_owned(),
                    after_millis: AFTER_MILLIS,
                },
            )
            .await;
            let _host =
                spawn_index_saga(&system, Subscription::stream(UPSTREAM), 1, &fired()).await;
            let events = wait_until_scheduled(&journal, &SagaId::new(SAGA)).await;
            wait_until_recorded(&offsets, TRIGGER_SEQUENCE).await;
            assert_eq!(
                count_schedule_transition(&events, "fired"),
                0,
                "the deadline must still be pending when the system stops"
            );
        });
        drop(runtime);
    })
    .join()
    .expect("the stopping system's thread must not panic");
}

#[tokio::test]
async fn a_pending_deadline_reaches_handle_after_a_restart_that_resumes_from_the_offset() {
    // Given: the deadline and the manager's offset are recorded, and then the
    // whole system stops.
    let inner = InMemoryEventStore::default();
    let offsets = InMemoryOffsetStore::default();
    run_until_recorded_then_stop(&inner, &offsets);

    // When: the system restarts over the same store, its subscription floored
    // at the beginning as before.
    let probe = ProbeJournal::recording(inner);
    let system = system_over(&probe, &offsets).await;
    let fired = fired();
    let _host = spawn_index_saga(&system, Subscription::stream(UPSTREAM), 1, &fired).await;

    // Then
    let seen = wait_until_fired(
        &fired,
        1,
        "with the trigger behind the offset, only the index reading the \
         schedule fact can bring the instance back for its deadline",
    )
    .await;
    assert_eq!(
        seen,
        vec!["after-resume".to_owned()],
        "the deadline that reaches handle is the one the stopped system persisted"
    );
    let asked_from: Vec<Option<u64>> = probe
        .queries()
        .iter()
        .filter(|query| query.stream_key.as_deref() == Some(UPSTREAM))
        .map(|query| query.from_stream_sequence)
        .collect();
    assert!(
        !asked_from.is_empty(),
        "the restarted manager must have polled its source"
    );
    assert!(
        asked_from
            .iter()
            .all(|from| from.is_some_and(|from| from > TRIGGER_SEQUENCE)),
        "the restarted manager resumes above its offset and never reads the \
         trigger again, so it is not what woke the instance; asked from \
         {asked_from:?}"
    );
}
