//! What the index holds is what the schedule facts leave pending, for this saga
//! type only.
//!
//! ```gherkin
//!   Scenario: the index folds schedule and lifecycle facts
//!     Given saga streams holding scheduled, cancelled, fired and ended facts,
//!           and a scheduled fact another saga type's message type names
//!     When the index rebuilds
//!     Then it holds one deadline per (saga, timer name) still pending:
//!          a re-set name replaces the earlier deadline, a cancelled or fired
//!          name is gone, an ended saga holds nothing, and another saga type's
//!          fact is not this index's
//!     And it asks for the activation of each one that is due, at the due
//!          time the latest scheduled fact names
//! ```
//!
//! Every deadline here is already due, so the first tick after the rebuild asks
//! for all of them at once. The rebuild's count is the barrier for what was
//! *not* asked for: it is reported before any request, and it says how many
//! deadlines the index holds.

#[path = "common/helpers.rs"]
mod common;
#[path = "common/deadline_index.rs"]
mod deadline_index;
use deadline_index::{
    due_at_millis, field, fired, now_millis, seed_cancelled, seed_ended, seed_fired,
    seed_scheduled, seed_scheduled_as, spawn_index_saga, system_over, Capture, Fields, COUNT,
    DUE_AT_UNIX_MILLIS, FOREIGN_MESSAGE_TYPE, REMINDER, REQUESTED_ACTIVATION, SAGA_ID, TIMER,
};

use std::collections::BTreeSet;
use std::time::Duration;

use nitinol_persistence::store::InMemoryStore;
use nitinol_saga::{SagaId, Subscription};

const CANCELLED: &str = "deadline-index-fold-cancelled";
const FIRED: &str = "deadline-index-fold-fired";
const ENDED: &str = "deadline-index-fold-ended";
const FOREIGN: &str = "deadline-index-fold-foreign";
const RESET: &str = "deadline-index-fold-reset";
const TWO_NAMES: &str = "deadline-index-fold-two-names";
const OTHER_NAME_CANCELLED: &str = "deadline-index-fold-other-name-cancelled";

/// A delay that has already elapsed by the time the index reads the fact.
const ELAPSED: Duration = Duration::ZERO;

#[tokio::test]
async fn the_index_holds_the_pending_deadlines_of_its_own_saga_type_and_asks_for_the_due_ones() {
    // Given
    let capture = Capture::installed();
    let store = InMemoryStore::default();
    let system = system_over(&store).await;
    let journal = system.persistence().event_store();
    let anchor = now_millis() - 60_000;

    let cancelled = SagaId::new(CANCELLED);
    seed_scheduled(&journal, &cancelled, 1, REMINDER, ELAPSED, "c", anchor).await;
    seed_cancelled(&journal, &cancelled, 2, REMINDER).await;

    let fired_saga = SagaId::new(FIRED);
    seed_scheduled(&journal, &fired_saga, 1, REMINDER, ELAPSED, "f", anchor).await;
    seed_fired(&journal, &fired_saga, 2, REMINDER).await;

    let ended = SagaId::new(ENDED);
    seed_scheduled(&journal, &ended, 1, "a", ELAPSED, "e-a", anchor).await;
    seed_scheduled(&journal, &ended, 2, "b", ELAPSED, "e-b", anchor).await;
    seed_ended(&journal, &ended, 3).await;

    let foreign = SagaId::new(FOREIGN);
    seed_scheduled_as(
        &journal,
        &foreign,
        1,
        REMINDER,
        ELAPSED,
        "x",
        anchor,
        FOREIGN_MESSAGE_TYPE,
    )
    .await;

    let reset = SagaId::new(RESET);
    let first_anchor = anchor;
    let latest_anchor = anchor + 1_000;
    let latest_after = Duration::from_millis(250);
    seed_scheduled(&journal, &reset, 1, REMINDER, ELAPSED, "r-1", first_anchor).await;
    seed_scheduled(
        &journal,
        &reset,
        2,
        REMINDER,
        latest_after,
        "r-2",
        latest_anchor,
    )
    .await;

    let two_names = SagaId::new(TWO_NAMES);
    seed_scheduled(&journal, &two_names, 1, "a", ELAPSED, "t-a", anchor).await;
    seed_scheduled(&journal, &two_names, 2, "b", ELAPSED, "t-b", anchor).await;

    let other_name = SagaId::new(OTHER_NAME_CANCELLED);
    seed_scheduled(&journal, &other_name, 1, "a", ELAPSED, "o-a", anchor).await;
    seed_cancelled(&journal, &other_name, 2, "b").await;

    let pending: BTreeSet<(String, String)> = [
        (RESET, REMINDER),
        (TWO_NAMES, "a"),
        (TWO_NAMES, "b"),
        (OTHER_NAME_CANCELLED, "a"),
    ]
    .into_iter()
    .map(|(saga, timer)| (saga.to_owned(), timer.to_owned()))
    .collect();

    // When
    let _host = spawn_index_saga(
        &system,
        Subscription::stream("deadline-index-fold-unused-upstream"),
        1,
        &fired(),
    )
    .await;

    // Then: every pending deadline is asked for.
    let requests = capture
        .wait_for(
            REQUESTED_ACTIVATION,
            |records| requested(records).is_superset(&pending),
            "each pending, due deadline must be asked for",
        )
        .await;

    // And: the index held those and nothing else.
    let rebuilt = capture.rebuilt();
    let expected = pending.len().to_string();
    assert_eq!(
        rebuilt.first().and_then(|fields| field(fields, COUNT)),
        Some(expected.as_str()),
        "the rebuild holds one deadline per (saga, timer name) still pending: a \
         re-set name replaces, a cancelled or fired name is removed, an ended \
         saga holds nothing, and a fact naming another saga type's message type \
         is not this index's; the capture held {rebuilt:?}"
    );
    assert_eq!(
        requested(&requests),
        pending,
        "no activation is asked for a cancelled, fired, ended or foreign deadline"
    );

    // And: a re-set name is due when its latest fact says.
    let reset_due = due_at_millis(latest_anchor, latest_after).to_string();
    let reset_requests: Vec<Option<&str>> = requests
        .iter()
        .filter(|fields| field(fields, SAGA_ID) == Some(RESET))
        .map(|fields| field(fields, DUE_AT_UNIX_MILLIS))
        .collect();
    assert!(
        !reset_requests.is_empty() && reset_requests.iter().all(|due| *due == Some(&reset_due)),
        "the due time is the latest scheduled fact's anchor plus its delay \
         ({reset_due}), not the earlier fact's nor one measured from now; \
         requests for {RESET} named {reset_requests:?}"
    );
}

fn requested(records: &[Fields]) -> BTreeSet<(String, String)> {
    records
        .iter()
        .filter_map(|fields| {
            Some((
                field(fields, SAGA_ID)?.to_owned(),
                field(fields, TIMER)?.to_owned(),
            ))
        })
        .collect()
}
