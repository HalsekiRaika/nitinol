//! A refusal at the receiver is something the host can see.
//!
//! ```gherkin
//!   Scenario: the receiver refuses the command
//!     Given a delivery host and a receiver whose decider refuses every command
//!     When a decision is persisted and delivered
//!     Then the host does not record the decision's position as progress
//!     And it keeps asking the store for that same record
//! ```
//!
//! A dispatch with no answer could not keep this: nothing would come back to
//! distinguish "refused" from "carried out", the record would be treated as
//! finished, and the refusal would be lost. The observable consequence is the
//! progress that does not move, and the poll that keeps asking for the same
//! position.
//!
//! A refusal is a verdict on the command against that state, so re-issuing it
//! reproduces the verdict. Holding is therefore what the host does — not
//! skipping the record once it has decided the refusal is permanent.

#[path = "common/delivery.rs"]
mod delivery;
use delivery::{
    append_decided, in_memory_system, scope_of, settle, spawn_host, wait_until_loads,
    RecordingEventStore, RecordingOffsetStore, RefusedSaga,
};

use std::sync::Arc;

use nitinol_persistence::store::{EventStore, InMemoryEventStore};
use nitinol_persistence::AggregateId;
use nitinol_saga::SagaId;

#[tokio::test]
async fn a_refused_delivery_does_not_become_progress() {
    // Given
    let (system, _aggregates) = in_memory_system().await;
    let recording = RecordingEventStore::new(Arc::new(InMemoryEventStore::default()));
    let journal: Arc<dyn EventStore> = recording.clone();
    let offsets = RecordingOffsetStore::new();
    let saga_id = SagaId::new("instance-1");
    let target = AggregateId::new("refuser-rejection-observed");

    let position = append_decided(&journal, &saga_id, 1, target.as_str(), "op-1").await;

    // When
    let _host = spawn_host::<RefusedSaga>(&system, &journal, &offsets, 1).await;
    wait_until_loads(&recording, 4, "the host must keep polling while held").await;
    settle().await;

    // Then
    assert!(
        offsets
            .stored(&scope_of(0))
            .is_none_or(|held| held < position),
        "a record whose delivery was refused is not finished with, so its \
         position must not be recorded as progress"
    );
    assert!(
        !offsets
            .save_calls()
            .iter()
            .any(|(_, sequence)| *sequence >= position),
        "the host must not even ask the store to record a position at or past \
         the refused record"
    );
}

#[tokio::test]
async fn the_same_record_is_asked_for_again() {
    // Given
    let (system, _aggregates) = in_memory_system().await;
    let recording = RecordingEventStore::new(Arc::new(InMemoryEventStore::default()));
    let journal: Arc<dyn EventStore> = recording.clone();
    let offsets = RecordingOffsetStore::new();
    let saga_id = SagaId::new("instance-1");
    let target = AggregateId::new("refuser-asked-again");

    let position = append_decided(&journal, &saga_id, 1, target.as_str(), "op-1").await;

    // When
    let _host = spawn_host::<RefusedSaga>(&system, &journal, &offsets, 1).await;
    let queries = wait_until_loads(&recording, 4, "the host must keep polling while held").await;

    // Then
    for (nth, query) in queries.iter().enumerate() {
        assert_eq!(
            query.from_global_sequence,
            Some(position),
            "load call {nth} must still ask for the held record: a cursor that \
             moved past a refusal would leave the delivery undone with nothing \
             left pointing at it"
        );
    }
}
