//! "It already exists" is not an answer that something was carried out.
//!
//! ```gherkin
//!   Scenario: the target already exists
//!     Given a delivery whose target stream was already written by somebody else
//!     And no guarantee that the existing write is this same operation
//!     When the host judges the outcome of the delivery
//!     Then it does not treat the collision as a success
//!     And it does not record that record's position as progress
//! ```
//!
//! What the collision carries is the fact of the collision and nothing more:
//! which operation wrote the position that is already taken is not in it. So
//! the host cannot tell its own redelivery from somebody else's first write,
//! and reading the collision as success would have it record progress for a
//! command that was never carried out.
//!
//! The one success signal the host accepts is the receiver's own answer. A
//! receiver that must not carry an operation out twice recognises the repeat
//! from the operation id the domain put on the command and answers with
//! nothing written — which is an answer, not a collision.
//!
//! The collision is produced without a test double: a reference resolved before
//! anything was stored still addresses the stream's first position, and a
//! foreign write that lands in the meantime takes it.

#[path = "common/delivery.rs"]
mod delivery;
use delivery::{
    append_decided, append_row, in_memory_system, operations_of, scope_of, settle, spawn_host,
    stream_rows, wait_until_loads, DeliverySaga, Ledger, Recorded, RecordingEventStore,
    RecordingOffsetStore,
};

use nitinol_persistence::store::InMemoryEventStore;
use nitinol_persistence::AggregateId;
use nitinol_saga::SagaId;

/// The operation the foreign write recorded — deliberately not the one the
/// decision asks for, so nothing about the stored state says the collision is
/// a redelivery of this delivery.
const FOREIGN_OPERATION: &str = "op-written-by-someone-else";

#[tokio::test]
async fn a_collision_with_an_existing_target_is_not_treated_as_delivered() {
    // Given: a reference to the target, resolved and replayed while its stream
    // is still empty, so its next write addresses the stream's first position.
    let (system, aggregates) = in_memory_system().await;
    let target = AggregateId::new("ledger-already-created");
    let pinned = system.spawn_aggregate::<Ledger>(target.clone()).await;
    assert!(
        operations_of(&pinned).await.is_empty(),
        "the target's stream starts empty, and the reference has replayed it"
    );

    // And: somebody else writes that first position in the meantime.
    append_row(
        &aggregates,
        target.as_str(),
        1,
        &Recorded {
            operation: FOREIGN_OPERATION.to_owned(),
        },
    )
    .await;

    let recording = RecordingEventStore::new(InMemoryEventStore::default());
    let journal = recording.clone();
    let offsets = RecordingOffsetStore::new();
    let saga_id = SagaId::new("instance-1");
    let position = append_decided(&journal, &saga_id, 1, target.as_str(), "op-1").await;

    // When
    let _host = spawn_host::<DeliverySaga, _>(&system, &journal, &offsets, 1).await;
    wait_until_loads(&recording, 4, "the host must keep polling while held").await;
    settle().await;

    // Then
    assert!(
        offsets
            .stored(&scope_of(0))
            .is_none_or(|held| held < position),
        "the delivery was not carried out, so the record it came from must not \
         be recorded as finished with"
    );
    assert!(
        !offsets
            .save_calls()
            .iter()
            .any(|(_, sequence)| *sequence >= position),
        "the host must not even ask the store to record a position at or past \
         the record whose delivery collided"
    );

    let rows = stream_rows(&aggregates, target.as_str()).await;
    assert_eq!(
        rows.len(),
        1,
        "the first write stays intact and the collision adds nothing"
    );
    assert_eq!(
        serde_json::from_slice::<Recorded>(&rows[0].payload)
            .expect("the stored row is the foreign write")
            .operation,
        FOREIGN_OPERATION,
        "the operation the decision asked for was never carried out, which is \
         exactly why the collision must not be read as its success"
    );
}
