//! A pending deadline outlives the whole system that set it.
//!
//! ```gherkin
//!   Scenario: a deadline arrives without a new upstream event
//!     Given a saga has persisted a deadline, and then the whole system —
//!           manager and scheduler included — stops
//!     And no new upstream event for that correlation id is ever appended
//!     When the system restarts and the manager does not re-read its upstream
//!          (the subscription's floor sits at the position of the restart)
//!     Then the deadline reaches handle when it falls due
//! ```
//!
//! The floor is what takes the manager out of the picture: with it, the one
//! upstream record the instance was ever claimed by is never delivered again,
//! so the manager has no reason of its own to bring the instance back. What is
//! left is the schedule fact on the saga's own stream, and the read-side index
//! that reads it.

use super::deadline_index;
use deadline_index::{
    fired, run_until_scheduled_then_stop, spawn_index_saga, system_over, wait_until_fired, Trigger,
};

use nitinol_persistence::store::InMemoryStore;
use nitinol_saga::Subscription;

const UPSTREAM: &str = "deadline-index-restart-upstream";
const SAGA: &str = "deadline-index-restart-1";

/// The upstream sequence of the one trigger, which is also where the restarted
/// system's floor is put.
const RESTART_POSITION: u64 = 1;

/// Long enough that the deadline is still pending when the first system stops.
const AFTER_MILLIS: u64 = 2_000;

#[tokio::test]
async fn a_pending_deadline_reaches_handle_after_a_restart_with_no_upstream_resweep() {
    // Given: the deadline is persisted, and then the whole system stops.
    let store = InMemoryStore::default();
    let before_restart = run_until_scheduled_then_stop(
        &store,
        UPSTREAM,
        Trigger {
            saga: SAGA.to_owned(),
            note: "after-restart".to_owned(),
            after_millis: AFTER_MILLIS,
        },
    );
    assert!(
        before_restart.is_empty(),
        "the deadline must not have fired before the system stopped; fired={before_restart:?}"
    );

    // When: the system restarts over the same store, its manager floored at the
    // restart so the trigger is never re-read.
    let system = system_over(&store).await;
    let fired = fired();
    let _host = spawn_index_saga(
        &system,
        Subscription::stream(UPSTREAM).with_after(RESTART_POSITION),
        1,
        &fired,
    )
    .await;

    // Then
    let seen = wait_until_fired(
        &fired,
        1,
        "with no upstream record left to wake the instance, only the index \
         reading the schedule fact can bring it back for its deadline",
    )
    .await;
    assert_eq!(
        seen,
        vec!["after-restart".to_owned()],
        "the deadline that reaches handle after the restart is the one the \
         stopped system persisted, carrying the message it was set with"
    );
}
