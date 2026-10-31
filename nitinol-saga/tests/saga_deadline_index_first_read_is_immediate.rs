//! The deadline index reads the journal as soon as it starts, not one poll
//! interval later.
//!
//! ```gherkin
//!   Scenario: the index's first read does not wait for the interval
//!     Given a saga whose own stream already records a deadline that has
//!           fallen due
//!     And a host whose poll interval is far longer than the default
//!     When the host starts
//!     Then the index has read the journal and asked for that saga's
//!          activation well inside the interval
//! ```
//!
//! The deadline is seeded before the host starts and is already past due, so
//! the first read is the only thing standing between the start and the
//! request: an index that slept through its interval first asks no sooner than
//! that interval.
//!
//! This binary hosts one deadline saga, which is what the process-wide
//! capture of the index's records needs.
//!
//! Run with:
//! `cargo test -p nitinol-saga --test saga_deadline_index_first_read_is_immediate`.

#[path = "common/helpers.rs"]
mod common;
#[path = "common/deadline_index.rs"]
mod deadline_index;
use deadline_index::{
    field, fired, now_millis, seed_scheduled, spawn_index_saga_polling, system_over, Capture,
    REMINDER, REQUESTED_ACTIVATION, SAGA_ID,
};

use std::time::{Duration, Instant};

use nitinol_persistence::store::InMemoryStore;
use nitinol_saga::{SagaId, Subscription};

const SAGA: &str = "deadline-index-first-read-is-immediate-1";

/// Far longer than the default, so a first read that waited for it cannot pass
/// for one that did not.
const POLL: Duration = Duration::from_secs(2);

/// Far shorter than [`POLL`], and still loose enough for a loaded machine.
const WELL_INSIDE: Duration = Duration::from_millis(500);

/// A delay that had elapsed long before the host starts.
const ELAPSED: Duration = Duration::ZERO;

#[tokio::test]
async fn the_index_asks_for_a_deadline_already_due_without_waiting_for_its_interval() {
    // Given
    let capture = Capture::installed();
    let store = InMemoryStore::default();
    let system = system_over(&store).await;
    let journal = system.persistence().event_store();
    seed_scheduled(
        &journal,
        &SagaId::new(SAGA),
        1,
        REMINDER,
        ELAPSED,
        "already-due",
        now_millis() - 60_000,
    )
    .await;

    // When
    let started = Instant::now();
    let _host = spawn_index_saga_polling(
        &system,
        Subscription::stream("deadline-index-first-read-upstream"),
        1,
        &fired(),
        POLL,
    )
    .await;
    let spawned_in = started.elapsed();
    capture
        .wait_for(
            REQUESTED_ACTIVATION,
            |records| {
                records
                    .iter()
                    .any(|fields| field(fields, SAGA_ID) == Some(SAGA))
            },
            "a deadline already due must be asked for",
        )
        .await;
    let asked_in = started.elapsed();

    // Then
    //
    // A failure here has come up once under the load of a whole-workspace test
    // run and not again in 60 runs under eight busy loops, so the message
    // carries what the next one needs: how long the spawn itself took, and
    // the records of the two paths that cost exactly one interval — a first
    // read that failed, or a first turn that panicked and was rebuilt.
    assert!(
        asked_in < WELL_INSIDE,
        "an index polling every {POLL:?} asked for a deadline already due \
         {asked_in:?} after the host started (the spawn alone took \
         {spawned_in:?}); its first read waited for the interval. rebuilt: \
         {rebuilt:?}; read failures: {read_failures:?}; panics: {panics:?}",
        rebuilt = capture.rebuilt(),
        read_failures = capture.records(COULD_NOT_READ),
        panics = capture.records(PARTITION_PANICKED),
    );
}

/// The record a partition writes when its read of the journal failed and it
/// will try again on the next tick.
const COULD_NOT_READ: &str =
    "saga deadline index could not read the journal; retrying on the next tick";

/// The record a partition writes when a turn panicked and supervision will
/// rebuild it.
const PARTITION_PANICKED: &str =
    "a saga deadline index partition panicked; supervision will restart it";
