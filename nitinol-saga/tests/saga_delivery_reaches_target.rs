//! A persisted decision becomes a command at its target.
//!
//! ```gherkin
//!   Scenario: a decision persisted, a command delivered
//!     Given a saga that declares a mapping, and a delivery host for it
//!     When a decision event is persisted
//!     Then the command the mapping names reaches the target aggregate
//!     And the host records the decision's position as progress
//!
//!   Scenario: a saga that maps a decision to nothing
//!     Given a saga that declares no mapping
//!     When a decision event is persisted
//!     Then nothing is delivered
//!     And progress still moves past the decision, because nothing is owed
//! ```
//!
//! The receiving aggregate in these tests is an ordinary `Aggregate` plus a
//! `Decider`, and nothing else: it implements no trait the delivery host
//! introduced, and the host reaches it through the same reference any other
//! caller would. That is how "the receiver's code does not change" is kept
//! observable — a host that required something of its targets could not
//! address this one.
//!
//! Nothing drives a saga incarnation here. The host's whole input is the store,
//! so a decision is seeded by appending the row a saga would have written.

#[path = "common/delivery.rs"]
mod delivery;
use delivery::{
    append_decided, append_noted, in_memory_system, operations_of, scope_of, settle, spawn_host,
    wait_until_operations, wait_until_stored, DeliverySaga, Ledger, RecordingOffsetStore,
    SilentSaga,
};

use std::sync::Arc;

use nitinol_persistence::store::{EventStore, InMemoryEventStore};
use nitinol_persistence::AggregateId;
use nitinol_saga::SagaId;

#[tokio::test]
async fn a_persisted_decision_reaches_the_target_aggregate() {
    // Given
    let (system, _aggregates) = in_memory_system().await;
    let journal: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let offsets = RecordingOffsetStore::new();
    let saga_id = SagaId::new("instance-1");
    let target = AggregateId::new("ledger-reaches-target");

    // When
    let position = append_decided(&journal, &saga_id, 1, target.as_str(), "op-1").await;
    let _host = spawn_host::<DeliverySaga>(&system, &journal, &offsets, 1).await;

    // Then
    let ledger = system.aggregate_proxy::<Ledger>(target);
    assert_eq!(
        wait_until_operations(&ledger, 1, "the decision must reach the ledger").await,
        vec!["op-1".to_owned()],
        "the command the mapping named must be carried out by the aggregate the \
         decision addressed"
    );

    assert_eq!(
        wait_until_stored(
            &offsets,
            &scope_of(0),
            position,
            "the delivered decision's position must be recorded"
        )
        .await,
        position,
        "progress is recorded at the position of the record whose deliveries \
         all succeeded"
    );
}

#[tokio::test]
async fn a_decision_that_maps_to_nothing_delivers_nothing_and_still_advances() {
    // Given
    let (system, _aggregates) = in_memory_system().await;
    let journal: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let offsets = RecordingOffsetStore::new();
    let saga_id = SagaId::new("instance-1");
    let target = AggregateId::new("ledger-maps-to-nothing");

    // When
    let position = append_decided(&journal, &saga_id, 1, target.as_str(), "op-1").await;
    let _host = spawn_host::<SilentSaga>(&system, &journal, &offsets, 1).await;

    // Then
    let stored = wait_until_stored(
        &offsets,
        &scope_of(0),
        position,
        "a record owing nothing must not hold the cursor",
    )
    .await;
    assert_eq!(stored, position);

    let ledger = system.aggregate_proxy::<Ledger>(target);
    assert!(
        operations_of(&ledger).await.is_empty(),
        "a saga declaring no mapping delivers nothing, so the aggregate the \
         decision names must have been left alone"
    );
}

#[tokio::test]
async fn progress_moves_past_rows_the_subscription_leaves_out() {
    // Given: a journal where rows of another event type outnumber the
    // decisions, which is the normal shape of a saga's own stream.
    let (system, _aggregates) = in_memory_system().await;
    let journal: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    let offsets = RecordingOffsetStore::new();
    let saga_id = SagaId::new("instance-1");
    let target = AggregateId::new("ledger-with-gaps");

    append_noted(&journal, &saga_id, 1).await;
    let decision = append_decided(&journal, &saga_id, 2, target.as_str(), "op-1").await;
    append_noted(&journal, &saga_id, 3).await;
    append_noted(&journal, &saga_id, 4).await;

    // When
    let _host = spawn_host::<DeliverySaga>(&system, &journal, &offsets, 1).await;

    // Then
    let ledger = system.aggregate_proxy::<Ledger>(target);
    wait_until_operations(&ledger, 1, "the one decision must be delivered").await;
    settle().await;

    let stored = offsets
        .stored(&scope_of(0))
        .expect("the host must have recorded progress");
    assert_eq!(
        stored, decision,
        "progress is the position of the last record the subscription returned \
         and the host finished with. Holding it below that because the rows \
         after it were filtered out would make the store re-scan the same tail \
         on every poll"
    );
}
