//! A record is finished with only when every delivery it implies is.
//!
//! ```gherkin
//!   Scenario: one decision, two deliveries, one of them refused
//!     Given a decision whose mapping names two deliveries
//!     And the first is accepted while the second is refused
//!     When the host judges the record
//!     Then it does not record that record's position as progress
//!     And it does not deliver the decisions behind it
//! ```
//!
//! Part of a record's work landing is the case where deciding the position and
//! deciding the outcome separately goes wrong: it is tempting to read "a
//! delivery succeeded" as progress. The position and the outcome are projected
//! from one judgement of the record, so a record with anything still owed
//! cannot be read as finished however much of it went through.
//!
//! The accepted delivery is repeated on every redelivery of the held record,
//! which is what the receiver's idempotency is for: the operation id says it is
//! the same operation, and nothing is written a second time.

#[path = "common/delivery.rs"]
mod delivery;
use delivery::{
    append_decided, in_memory_system, operations_of, scope_of, settle, spawn_host,
    wait_until_operations, PartlyRefusedSaga, RecordingOffsetStore, REFUSED_SUFFIX,
};

use nitinol_persistence::store::InMemoryEventStore;
use nitinol_persistence::AggregateId;
use nitinol_saga::SagaId;

#[tokio::test]
async fn a_partly_delivered_record_is_not_progress() {
    // Given
    let (system, aggregates) = in_memory_system().await;
    let journal = InMemoryEventStore::default();
    let offsets = RecordingOffsetStore::new();
    let saga_id = SagaId::new("instance-1");
    let target = AggregateId::new("ledger-partly-delivered");

    let held = append_decided(&journal, &saga_id, 1, target.as_str(), "op-1").await;
    append_decided(&journal, &saga_id, 2, target.as_str(), "op-2").await;

    // When
    let _host = spawn_host::<PartlyRefusedSaga, _>(&system, &journal, &offsets, 1).await;

    // Then: the delivery that can land, lands.
    let ledger = system.aggregate_proxy::<delivery::Ledger>(target.clone());
    assert_eq!(
        wait_until_operations(&ledger, 1, "the accepted delivery must go through").await,
        vec!["op-1".to_owned()],
    );

    settle().await;

    // And: the record it came from is not finished with.
    assert!(
        offsets
            .stored(&scope_of(0))
            .is_none_or(|position| position < held),
        "one of the record's deliveries is still owed, so its position must not \
         be recorded as progress"
    );
    assert!(
        !offsets
            .save_calls()
            .iter()
            .any(|(_, sequence)| *sequence >= held),
        "the host must not offer the store a position at or past the record it \
         has not finished: {:?}",
        offsets.save_calls()
    );

    // And: nothing behind it is delivered.
    assert_eq!(
        operations_of(&ledger).await,
        vec!["op-1".to_owned()],
        "the decision behind the held record must wait: a cursor that walked \
         past the held record would leave its refused delivery with nothing \
         pointing at it"
    );

    // And: the repeated attempts wrote nothing more at the accepted receiver.
    assert_eq!(
        delivery::stream_rows(&aggregates, target.as_str())
            .await
            .len(),
        1,
        "the accepted delivery is re-issued while the record is held, and the \
         receiver recognises the repeat from the operation id"
    );
    assert!(
        delivery::stream_rows(&aggregates, &format!("{}{REFUSED_SUFFIX}", target.as_str()))
            .await
            .is_empty(),
        "nothing was ever accepted at the refusing receiver"
    );
}
