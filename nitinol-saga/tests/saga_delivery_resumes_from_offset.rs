//! Where the host starts, and what a restart costs.
//!
//! ```gherkin
//!   Scenario: a host starting with progress already recorded
//!     Given a journal of two decisions and a recorded position at the first
//!     When the host starts
//!     Then it delivers the second decision
//!     And it does not re-read the first
//!
//!   Scenario: a delivery that happened and a position that was never recorded
//!     Given a decision whose command the receiver already carried out
//!     And no recorded position for it
//!     When the host starts
//!     Then it delivers that command again
//!     And the receiver's idempotency keeps the business effect to one
//!     And the decision behind it, still undelivered, goes through
//! ```
//!
//! The second scenario is the crash this design accepts: a delivery is durable
//! at the receiver before the position behind it is durable at the host, so a
//! crash in between costs a redelivery and never a loss. Nothing on the host
//! suppresses the repeat — the receiver recognises it from the operation id the
//! domain put on the command, and answers with nothing written.

#[path = "common/delivery.rs"]
mod delivery;
use delivery::{
    append_decided, append_row, in_memory_system, operations_of, scope_of, settle, spawn_host,
    stream_rows, wait_until_operations, wait_until_stored, DeliverySaga, Ledger, Recorded,
    RecordingOffsetStore,
};

use nitinol_persistence::AggregateId;
use nitinol_saga::SagaId;

#[tokio::test]
async fn a_host_resumes_above_the_recorded_position() {
    // Given
    let offsets = RecordingOffsetStore::new();
    let (system, journal) = in_memory_system(&offsets).await;
    let saga_id = SagaId::new("instance-1");
    let target = AggregateId::new("ledger-resumes");

    let first = append_decided(&journal, &saga_id, 1, target.as_str(), "op-1").await;
    let second = append_decided(&journal, &saga_id, 2, target.as_str(), "op-2").await;
    offsets.put(&scope_of(0), first);

    // When
    let _host = spawn_host::<DeliverySaga>(&system, 1).await;

    // Then
    let ledger = system.aggregate_proxy::<Ledger>(target);
    assert_eq!(
        wait_until_operations(&ledger, 1, "the undelivered decision must go through").await,
        vec!["op-2".to_owned()],
        "the host resumes above the recorded position, so the decision already \
         accounted for is not read again"
    );
    assert_eq!(
        wait_until_stored(
            &offsets,
            &scope_of(0),
            second,
            "progress must move to the newly delivered decision"
        )
        .await,
        second,
    );

    settle().await;
    assert_eq!(
        operations_of(&ledger).await,
        vec!["op-2".to_owned()],
        "and it stays that way: the recorded position is where reading starts, \
         not a hint"
    );
}

#[tokio::test]
async fn a_lost_position_redelivers_without_doubling_the_effect() {
    // Given: the state a crash between a delivery and its recording leaves —
    // the receiver has the operation, the host has no position.
    let offsets = RecordingOffsetStore::new();
    let (system, journal) = in_memory_system(&offsets).await;
    let saga_id = SagaId::new("instance-1");
    let target = AggregateId::new("ledger-lost-position");

    append_row(
        &journal,
        target.as_str(),
        1,
        &Recorded {
            operation: "op-1".to_owned(),
        },
    )
    .await;

    append_decided(&journal, &saga_id, 1, target.as_str(), "op-1").await;
    let second = append_decided(&journal, &saga_id, 2, target.as_str(), "op-2").await;
    assert_eq!(
        offsets.stored(&scope_of(0)),
        None,
        "the position the crash lost was never recorded"
    );

    // When
    let _host = spawn_host::<DeliverySaga>(&system, 1).await;

    // Then
    let ledger = system.aggregate_proxy::<Ledger>(target.clone());
    assert_eq!(
        wait_until_operations(
            &ledger,
            2,
            "the decision the crash left undone must go through"
        )
        .await,
        vec!["op-1".to_owned(), "op-2".to_owned()],
        "the command already carried out is delivered again and accepted, and \
         the one that never was is delivered too"
    );
    assert_eq!(
        wait_until_stored(
            &offsets,
            &scope_of(0),
            second,
            "progress must reach the last decision"
        )
        .await,
        second,
    );

    settle().await;
    assert_eq!(
        stream_rows(&journal, target.as_str()).await.len(),
        2,
        "the redelivered command wrote nothing: one row for the operation the \
         crash left behind, one for the operation that followed it"
    );
}
