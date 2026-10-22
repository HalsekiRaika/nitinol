//! Losing the index costs time, never a deadline.
//!
//! ```gherkin
//!   Scenario: losing the index only makes it slower
//!     Given several sagas with pending deadlines
//!     When the index's contents (and its offset, if it had one) are thrown
//!          away and the index is restarted
//!     Then the index is rebuilt from the schedule facts, and every pending
//!          deadline is restored
//! ```
//!
//! The index is a cache of the schedule facts, so throwing it away is what a
//! supervised restart of its partition does: the incarnation that comes back
//! holds nothing and reads the facts again from the start of the journal. The
//! restart here is caused by the journal panicking on the index's second
//! schedule load — after the first incarnation has rebuilt, and long before any
//! deadline is due — so every deadline that reaches `handle` afterwards was
//! asked for by the incarnation that rebuilt.
//!
//! Each incarnation reports its rebuild once, with the number of deadlines it
//! restored; two reports with the full count are the two rebuilds.

#[path = "common/helpers.rs"]
mod common;
#[path = "common/deadline_index.rs"]
mod deadline_index;
use deadline_index::{
    field, fired, now_millis, probe_system, seed_scheduled, spawn_index_saga, wait_until_fired,
    Capture, ProbeJournal, COUNT, REBUILT, REMINDER,
};

use std::time::Duration;

use nitinol_persistence::store::InMemoryEventStore;
use nitinol_saga::{SagaId, Subscription};

const SAGAS: [&str; 3] = [
    "deadline-index-rebuild-1",
    "deadline-index-rebuild-2",
    "deadline-index-rebuild-3",
];

/// Far enough out that the restart below has happened before any is due.
const AFTER: Duration = Duration::from_millis(1_500);

/// The index's schedule load that panics: the first incarnation's second tick.
const FAILING_SCHEDULE_LOAD: usize = 2;

#[tokio::test]
async fn a_restarted_index_rebuilds_from_the_schedule_facts_and_restores_every_deadline() {
    // Given
    let capture = Capture::installed();
    let probe = ProbeJournal::panicking_on_schedule_load(
        InMemoryEventStore::default(),
        FAILING_SCHEDULE_LOAD,
    );
    let journal = probe.journal();
    let scheduled_at = now_millis();
    for saga in SAGAS {
        seed_scheduled(
            &journal,
            &SagaId::new(saga),
            1,
            REMINDER,
            AFTER,
            saga,
            scheduled_at,
        )
        .await;
    }
    let system = probe_system(&probe).await;

    // When: the index starts, rebuilds, and its partition is restarted.
    let fired = fired();
    let _host = spawn_index_saga(
        &system,
        Subscription::stream("deadline-index-rebuild-unused-upstream"),
        1,
        &fired,
    )
    .await;

    // Then: every pending deadline is restored.
    let mut seen = wait_until_fired(
        &fired,
        SAGAS.len(),
        "every deadline must still reach handle after the index lost its contents",
    )
    .await;
    seen.sort();
    assert_eq!(
        seen,
        SAGAS.map(str::to_owned).to_vec(),
        "each pending deadline reaches handle once, carrying its own message"
    );

    // And: the index was rebuilt from the facts, once per incarnation.
    let rebuilt = capture.rebuilt();
    let counts: Vec<Option<&str>> = rebuilt.iter().map(|fields| field(fields, COUNT)).collect();
    let expected = SAGAS.len().to_string();
    assert_eq!(
        counts,
        vec![Some(expected.as_str()), Some(expected.as_str())],
        "the first incarnation and the restarted one must each report `{REBUILT}` \
         once with all {expected} pending deadlines: an incarnation that kept \
         its predecessor's contents or position would report no rebuild, or a \
         smaller one; the capture held {rebuilt:?}"
    );
}
