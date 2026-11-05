//! The index wakes for a deadline that falls before its next poll.
//!
//! ```gherkin
//!   Scenario: a deadline due between two polls
//!     Given a host whose poll interval is long, and a manager that is stopped
//!     And a pending deadline the index has read on its first turn
//!     When the deadline falls due before the next poll would come
//!     Then the index asks for the saga's activation when the deadline falls
//!          due, not at the next poll
//! ```
//!
//! The deadline is seeded after the host has started, so the index's first
//! turn, which comes as soon as it starts, is not what reads it: the turn one
//! poll interval after the start is, and the poll after that is a whole
//! interval later. A stopped manager keeps the request visible without an
//! instance to fire it.

#[path = "common/helpers.rs"]
mod common;
#[path = "common/deadline_index.rs"]
mod deadline_index;
use deadline_index::{
    due_at_millis, field, fired, now_millis, seed_scheduled, spawn_index_saga_polling, system_over,
    Capture, PROBE_INTERVAL, PROBE_TIMEOUT, REMINDER, REQUESTED_ACTIVATION, SAGA_ID,
};

use std::time::{Duration, Instant};

use nitinol_persistence::store::InMemoryStore;
use nitinol_saga::{SagaId, Subscription};

const SAGA: &str = "deadline-index-wakes-at-deadline-1";
const POLL: Duration = Duration::from_secs(2);
/// After the turn that reads the deadline (one poll interval after the start)
/// and before the poll that follows it.
const DUE_IN: Duration = Duration::from_millis(2_600);
const LATE_BY: i64 = 800;

#[tokio::test]
async fn a_deadline_before_the_next_poll_is_asked_for_when_it_falls_due() {
    // Given
    let capture = Capture::installed();
    let store = InMemoryStore::default();
    let system = system_over(&store).await;
    let journal = system.persistence().event_store();
    let host = spawn_index_saga_polling(
        &system,
        Subscription::stream("deadline-index-wakes-upstream"),
        1,
        &fired(),
        POLL,
    )
    .await;
    let ps = system.process_system();
    host.manager()
        .stop()
        .await
        .expect("stopping a running manager must be accepted");
    let stopped_by = Instant::now() + PROBE_TIMEOUT;
    while ps.lookup(host.manager().pid()).await.is_some() {
        assert!(
            Instant::now() < stopped_by,
            "the manager must leave the registry once it is stopped"
        );
        tokio::time::sleep(PROBE_INTERVAL).await;
    }

    // When
    let anchor = now_millis();
    seed_scheduled(
        &journal,
        &SagaId::new(SAGA),
        1,
        REMINDER,
        DUE_IN,
        "between-polls",
        anchor,
    )
    .await;

    // Then
    capture
        .wait_for(
            REQUESTED_ACTIVATION,
            |records| {
                records
                    .iter()
                    .any(|fields| field(fields, SAGA_ID) == Some(SAGA))
            },
            "a deadline that falls due must be asked for",
        )
        .await;
    let late = now_millis() - due_at_millis(anchor, DUE_IN);
    assert!(
        late < LATE_BY,
        "the request came {late} ms after the deadline fell due; the poll that \
         follows the first turn is {} ms away, so waiting for it is the failure",
        POLL.as_millis()
    );
}
