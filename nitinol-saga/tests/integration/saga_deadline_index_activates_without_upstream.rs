//! Replay re-arms an unfired deadline even when nothing upstream wakes the
//! instance.
//!
//! The counterpart of `replay_rearms_an_unfired_deadline_and_fires_it` in
//! `saga_deadline_scheduler.rs`, which needs one inert upstream record to make
//! the incarnation exist at all. Here the host's one source is a stream nothing
//! is ever written to, so there is no record for the manager to spawn the
//! instance on: the schedule fact is the whole of the state, and the index
//! reading it is what brings the instance back to replay it.
//!
//! The seeded payload is written with the message type's own codec, which the
//! re-armed firing is read back through: the note reaching `handle` is the
//! message the fact holds, not one the test supplied a second way.

use super::deadline_index;
use deadline_index::{
    fired, now_millis, seed_scheduled, spawn_index_saga, system_over, wait_until_fired, REMINDER,
};

use std::time::Duration;

use nitinol_persistence::store::InMemoryStore;
use nitinol_saga::{SagaId, Subscription};

/// A stream key no row is ever appended under.
const UNUSED_UPSTREAM: &str = "deadline-index-unused-upstream";

#[tokio::test]
async fn replay_rearms_an_unfired_deadline_with_no_upstream_record_and_fires_it() {
    // Given: the state a crash leaves behind — an unfired schedule fact whose
    // due time is still ahead, and nothing upstream.
    let store = InMemoryStore::default();
    let system = system_over(&store).await;
    let journal = system.persistence().event_store();
    let saga_id = SagaId::new("deadline-index-no-boot-record-1");
    seed_scheduled(
        &journal,
        &saga_id,
        1,
        REMINDER,
        Duration::from_millis(400),
        "survivor",
        now_millis(),
    )
    .await;

    // When
    let fired = fired();
    let _host = spawn_index_saga(&system, Subscription::stream(UNUSED_UPSTREAM), 1, &fired).await;

    // Then
    let seen = wait_until_fired(
        &fired,
        1,
        "an instance no upstream record claims must still be brought back for \
         its pending deadline, re-armed from replay and fired",
    )
    .await;
    assert_eq!(
        seen,
        vec!["survivor".to_owned()],
        "the re-armed deadline must carry the message the schedule fact holds"
    );
}
