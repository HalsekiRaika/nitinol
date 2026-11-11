//! A pending deadline is delivered by whichever system hosts the saga type next.
//!
//! ```gherkin
//!   Scenario: a deadline delivered after the hosting moved
//!     Given two systems over one shared store, and a saga that persisted a
//!           deadline on the old one before the old one stopped
//!     And no new upstream event for that correlation id is ever appended
//!     When the new system hosts the same saga type
//!     Then the deadline reaches handle on the new system when it falls due
//! ```
//!
//! On a single node a change of hosting is a resumption somewhere else: the
//! new system already exists while the old one runs, shares nothing with it
//! but the store, and starts hosting only once the old one is gone. Its manager
//! is floored past the one upstream record, so the deadline can only reach it
//! through the schedule fact the old system left on the saga's own stream.

use super::deadline_index;
use deadline_index::{
    fired, run_until_scheduled_then_stop, spawn_index_saga, system_over, wait_until_fired, Trigger,
};

use nitinol_persistence::store::InMemoryStore;
use nitinol_saga::Subscription;

const UPSTREAM: &str = "deadline-index-handover-upstream";
const SAGA: &str = "deadline-index-handover-1";

/// The upstream sequence of the one trigger the old system was handed.
const LAST_UPSTREAM_SEQUENCE: u64 = 1;

/// Long enough that the deadline is still pending when the old system stops.
const AFTER_MILLIS: u64 = 2_000;

#[tokio::test]
async fn a_pending_deadline_reaches_handle_on_the_system_that_hosts_the_saga_next() {
    // Given: two systems over one shared store.
    let store = InMemoryStore::default();
    let new_system = system_over(&store).await;

    // And: the old one persists the deadline and stops.
    let on_old_system = run_until_scheduled_then_stop(
        &store,
        UPSTREAM,
        Trigger {
            saga: SAGA.to_owned(),
            note: "handed-over".to_owned(),
            after_millis: AFTER_MILLIS,
        },
    );
    assert!(
        on_old_system.is_empty(),
        "the deadline must not have fired on the old system; fired={on_old_system:?}"
    );

    // When: the new system hosts the same saga type.
    let fired = fired();
    let _host = spawn_index_saga(
        &new_system,
        Subscription::stream(UPSTREAM).with_after(LAST_UPSTREAM_SEQUENCE),
        1,
        &fired,
    )
    .await;

    // Then
    let seen = wait_until_fired(
        &fired,
        1,
        "the new system must deliver the deadline the old one persisted",
    )
    .await;
    assert_eq!(
        seen,
        vec!["handed-over".to_owned()],
        "the deadline that reaches handle on the new system is the one the old \
         system persisted"
    );
}
