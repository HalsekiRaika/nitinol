//! A recording that fails does not turn undelivered work into done work.
//!
//! ```gherkin
//!   Scenario: the position cannot be recorded
//!     Given decisions whose deliveries succeed
//!     And a checkpoint store that refuses the first saves it is asked for
//!     When the host carries on
//!     Then no undelivered command is skipped
//!     And once the store recovers, the finished deliveries' position is recorded
//! ```
//!
//! The recording is a cache of progress, not the record of the business facts:
//! those are the saga's own events and the receivers'. So a failed save must
//! never be read as "this stretch is done" — the host owes every delivery it
//! has not carried out, whatever the store said about the position.
//!
//! The reverse order is the one this design accepts: the host may be further
//! along than the store knows. A crash there costs a redelivery, which the
//! receiver absorbs, and never a delivery that nothing points at any more.

#[path = "common/delivery.rs"]
mod delivery;
use delivery::{
    append_decided, in_memory_system, scope_of, settle, spawn_host, wait_until_operations,
    wait_until_stored, DeliverySaga, Ledger, RecordingCheckpointStore,
};

use std::sync::Arc;

use nitinol_persistence::store::{EventStore, InMemoryEventStore};
use nitinol_persistence::AggregateId;
use nitinol_saga::SagaId;

#[tokio::test]
async fn a_failed_save_skips_nothing_and_is_retried() {
    // Given
    let (system, _aggregates) = in_memory_system().await;
    let journal: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let checkpoints = RecordingCheckpointStore::new();
    let saga_id = SagaId::new("instance-1");
    let target = AggregateId::new("ledger-save-failure");

    append_decided(&journal, &saga_id, 1, target.as_str(), "op-1").await;
    append_decided(&journal, &saga_id, 2, target.as_str(), "op-2").await;
    let third = append_decided(&journal, &saga_id, 3, target.as_str(), "op-3").await;

    checkpoints.fail_next_saves(3);

    // When
    let _host = spawn_host::<DeliverySaga>(&system, &journal, &checkpoints, 1).await;

    // Then: nothing was skipped on account of the store's refusals.
    let ledger = system.aggregate_proxy::<Ledger>(target);
    assert_eq!(
        wait_until_operations(&ledger, 3, "every decision must still be delivered").await,
        vec!["op-1".to_owned(), "op-2".to_owned(), "op-3".to_owned()],
        "a save the store refused says nothing about which deliveries were \
         carried out, so none of them may be treated as done"
    );

    // And: the save is retried until it lands.
    assert_eq!(
        wait_until_stored(
            &checkpoints,
            &scope_of(0),
            third,
            "the position must be recorded once the store recovers"
        )
        .await,
        third,
        "the host keeps the position it reached and re-offers it, rather than \
         letting the refusal lose it"
    );

    settle().await;

    assert!(
        checkpoints.save_calls().len() > 3,
        "the refused saves were followed by further attempts: {:?}",
        checkpoints.save_calls()
    );
    assert!(
        checkpoints
            .save_calls()
            .iter()
            .all(|(_, sequence)| *sequence <= third),
        "no save ever offers a position beyond the records the host has \
         finished with: {:?}",
        checkpoints.save_calls()
    );
}
