//! A recording that fails does not turn undelivered work into done work, and
//! does not stop the host that was refused.
//!
//! ```gherkin
//!   Scenario: the position cannot be recorded
//!     Given decisions whose deliveries succeed
//!     And an offset store that refuses the first saves it is asked for
//!     When the host carries on
//!     Then no undelivered command is skipped
//!
//!   Scenario: the host does not stop at a refusal
//!     Given a host that has had a save refused
//!     When a further decision is appended
//!     Then that decision is delivered too
//!
//!   Scenario: the save is made again at the next move forward
//!     Given a host whose store has started taking saves again
//!     When it next moves forward
//!     Then the position it reached is recorded
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
//!
//! The last two scenarios are the consumer's half of the offset contract's
//! clause on a save that is not taken: a refusal is the store's answer and not
//! an instruction, so the consumer neither stops nor forgets — it carries on and
//! offers the position again the next time it moves forward. A host that gave up
//! on a refusal would keep the first scenario and fail these two: nothing it had
//! already delivered would be lost, and nothing after it would ever happen.
//!
//! One host runs across the whole of it. The refusals and the recovery are two
//! states of the same running partition, so spawning a second host over a
//! healthy store would judge a different thing: whether a *fresh* host records,
//! which was never in doubt.

#[path = "common/delivery.rs"]
mod delivery;
use delivery::{
    append_decided, in_memory_system, scope_of, settle, spawn_host, wait_until_operations,
    wait_until_refused, wait_until_stored, DeliverySaga, Ledger, RecordingOffsetStore,
};

use nitinol_persistence::store::InMemoryEventStore;
use nitinol_persistence::AggregateId;
use nitinol_saga::SagaId;

/// The operations the receiver has carried out, as a comparable value.
fn carried_out(operations: &[&str]) -> Vec<String> {
    operations.iter().map(|op| (*op).to_owned()).collect()
}

#[tokio::test]
async fn a_refused_save_skips_nothing_and_stops_nothing() {
    // Given
    let (system, _aggregates) = in_memory_system().await;
    let journal = InMemoryEventStore::default();
    let offsets = RecordingOffsetStore::new();
    let saga_id = SagaId::new("instance-1");
    let target = AggregateId::new("ledger-save-failure");

    append_decided(&journal, &saga_id, 1, target.as_str(), "op-1").await;
    append_decided(&journal, &saga_id, 2, target.as_str(), "op-2").await;
    let third = append_decided(&journal, &saga_id, 3, target.as_str(), "op-3").await;

    offsets.fail_next_saves(3);

    // When
    let _host = spawn_host::<DeliverySaga, _>(&system, &journal, &offsets, 1).await;
    let ledger = system.aggregate_proxy::<Ledger>(target.clone());

    // Then: nothing was skipped on account of the store's refusals.
    assert_eq!(
        wait_until_operations(&ledger, 3, "every decision must still be delivered").await,
        carried_out(&["op-1", "op-2", "op-3"]),
        "a save the store refused says nothing about which deliveries were \
         carried out, so none of them may be treated as done"
    );

    // And: a refusal really did reach this host, so what follows is judged
    // after one rather than beside one.
    let refused = wait_until_refused(
        &offsets,
        1,
        "the store must answer the saves it was told to refuse with an error",
    )
    .await;
    assert!(
        refused
            .iter()
            .all(|(key, sequence)| *key == scope_of(0).as_str() && *sequence <= third),
        "the refusals are the host's own saves for its own partition: {refused:?}"
    );

    // And: the host did not stop. A decision appended after the refusal is
    // delivered by the same host, on the same partition.
    let fourth = append_decided(&journal, &saga_id, 4, target.as_str(), "op-4").await;

    assert_eq!(
        wait_until_operations(
            &ledger,
            4,
            "a decision appended after a refused save must still be delivered"
        )
        .await,
        carried_out(&["op-1", "op-2", "op-3", "op-4"]),
        "a refusal is the store's answer about a position, not an instruction \
         to the host: a host that stopped there would owe every delivery after \
         it for ever"
    );

    // And: the save is made again at the next move forward, once the store
    // takes one.
    assert_eq!(
        wait_until_stored(
            &offsets,
            &scope_of(0),
            fourth,
            "the position must be recorded once the store recovers"
        )
        .await,
        fourth,
        "the host keeps the position it reached and re-offers it, rather than \
         letting the refusal lose it"
    );

    settle().await;

    let save_calls = offsets.save_calls();
    assert!(
        save_calls.len() > offsets.refused_saves().len(),
        "the refused saves were followed by further attempts: {save_calls:?}"
    );
    assert!(
        save_calls.iter().all(|(_, sequence)| *sequence <= fourth),
        "no save ever offers a position beyond the records the host has \
         finished with: {save_calls:?}"
    );
}
