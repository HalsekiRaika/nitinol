//! What the kit catches — the clauses, shown failing against stores that break
//! them.
//!
//! ```gherkin
//! Scenario: a store whose numbering order and visibility order come apart
//!   Given a store that withholds its first append until a reader has passed it
//!   When the kit's concurrency verification is run
//!   Then it fails, naming OCC-5
//!
//! Scenario: a store that serves a batch one event short
//!   Given a store that hides the last event of a batch from the first load
//!   When the kit's deterministic verification is run
//!   Then it fails, naming OCC-3
//!
//! Scenario: a snapshot store that answers with a state no save ever wrote
//!   Given a store answering with the latest sequence and the first payload
//!   When the kit's snapshot verification is run
//!   Then it fails, naming SNAP-3
//!
//! Scenario: a checkpoint store that cannot tell "never started" from "at zero"
//!   Given a store answering an unsaved scope with a position of zero
//!   When the kit's deterministic checkpoint verification is run
//!   Then it fails, naming CKP-1
//!
//! Scenario: a provider that hands out a store of its own per call
//!   Given a provider answering every event-store request with a new store
//!   When the kit's provider verification is run
//!   Then it fails, naming PRV-1
//!
//! Scenario: a provider whose checkpoint slots cannot be told apart
//!   Given a provider over a store with one position for every key
//!   When the kit's slot-independence verification is run
//!   Then it fails, naming PRV-4
//! ```
//!
//! A suite that only ever passes says nothing about what it measures, and OCC-5
//! is the clause this matters most for: it was added because a backend can
//! break it while every stored byte looks right and no error is raised
//! anywhere.
//!
//! Both faults are constructed rather than raced. The withheld append waits on
//! the reader's own progress, and the short batch is short on the first load
//! whatever the scheduling, so neither test rests on winning a race — which is
//! also why a probabilistic stress run is not on its own evidence that the kit
//! would have caught anything.
//!
//! The snapshot and checkpoint faults below are constructed in the same sense:
//! each store answers the same way however a run is scheduled, so the clause
//! that catches it does so every time.
//!
//! `expected` matches the label the kit puts at the front of its message, which
//! is what tells a broken clause from a panic for any other reason.

#[path = "common/helpers.rs"]
mod common;
use common::{
    CheckpointFault, Fault, FaultyCheckpointStore, FaultyProvider, FaultySnapshotStore,
    FaultyStore, ProviderWithCollidingCheckpointSlots, SnapshotFault, TokioExecutor,
};

use nitinol_store_conformance::{
    checkpoint, provider, snapshot, verify_deterministic, verify_under_concurrency,
};

/// A `global_sequence` handed out before the commit it belongs to becomes
/// visible below a checkpoint the reader has already passed, so the event is
/// never read and nothing reports it. The concurrency clause must name that
/// silent loss OCC-5.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[should_panic(expected = "OCC-5")]
async fn delayed_visibility_is_reported_as_an_occ_5_violation() {
    verify_under_concurrency(
        &FaultyStore::injecting(Fault::DelayFirstAppendVisibility),
        &TokioExecutor,
    )
    .await;
}

/// A batch that becomes visible one event short breaks OCC-3's commit unit, and
/// no concurrency is needed to see it: the single-event appends of the earlier
/// clauses go through untouched, so the first clause to fail is the one that
/// reads a batch back.
#[tokio::test]
#[should_panic(expected = "OCC-3")]
async fn a_partially_visible_batch_is_reported_as_an_occ_3_violation() {
    verify_deterministic(&FaultyStore::injecting(
        Fault::WithholdLastEventOfMultiEventBatch,
    ))
    .await;
}

/// An aggregate that has never been snapshotted is a condition, not a failure:
/// it is what every aggregate activating for the first time looks like. A store
/// that raised an error there would stop an activation the contract has carrying
/// on from its default state.
#[tokio::test]
#[should_panic(expected = "SNAP-1")]
async fn failing_the_load_of_an_unsaved_aggregate_is_reported_as_a_snap_1_violation() {
    snapshot::verify(&FaultySnapshotStore::injecting(
        SnapshotFault::FailTheUnsavedAggregate,
    ))
    .await;
}

/// A store that answers with the first snapshot an aggregate ever saved has the
/// aggregate restore an old state and replay from the position that state was
/// taken at, with no error raised anywhere.
#[tokio::test]
#[should_panic(expected = "SNAP-3")]
async fn answering_with_a_superseded_snapshot_is_reported_as_a_snap_3_violation() {
    snapshot::verify(&FaultySnapshotStore::injecting(
        SnapshotFault::TheFirstSaveWins,
    ))
    .await;
}

/// The worse half of the same clause, and the reason it is about the pair rather
/// than about the sequence: a store answering with the latest sequence and an
/// earlier payload hands back a state no save ever wrote, and the aggregate then
/// replays from beyond the events that payload is missing. A clause that checked
/// the sequence alone would pass this store.
#[tokio::test]
#[should_panic(expected = "SNAP-3")]
async fn answering_with_a_sequence_and_a_payload_from_different_saves_is_reported_as_a_snap_3_violation(
) {
    snapshot::verify(&FaultySnapshotStore::injecting(
        SnapshotFault::TheLatestSequenceWithTheFirstPayload,
    ))
    .await;
}

/// A store with one slot for every aggregate has an aggregate restore another
/// aggregate's state.
#[tokio::test]
#[should_panic(expected = "SNAP-4")]
async fn one_slot_for_every_aggregate_is_reported_as_a_snap_4_violation() {
    snapshot::verify(&FaultySnapshotStore::injecting(
        SnapshotFault::OneSlotForEveryAggregate,
    ))
    .await;
}

/// A position of zero for a scope nothing has saved for cannot be told from a
/// consumer that really did stop at zero, which is the distinction a consumer
/// deciding where to resume from rests on.
#[tokio::test]
#[should_panic(expected = "CKP-1")]
async fn reading_an_unsaved_scope_as_zero_is_reported_as_a_ckp_1_violation() {
    checkpoint::verify_deterministic(&FaultyCheckpointStore::injecting(
        CheckpointFault::ReadTheUnsavedScopeAsZero,
    ))
    .await;
}

/// A consumer following a store-wide order passes records it has no handler for,
/// so the position it saves skips numbers. A store that took only the next one
/// would refuse every save such a consumer makes.
#[tokio::test]
#[should_panic(expected = "CKP-3")]
async fn refusing_a_position_that_skips_numbers_is_reported_as_a_ckp_3_violation() {
    checkpoint::verify_deterministic(&FaultyCheckpointStore::injecting(
        CheckpointFault::TakeOnlyTheSequenceAfterTheStoredOne,
    ))
    .await;
}

/// A store with one slot for every scope has each consumer resume from wherever
/// another one had got to.
#[tokio::test]
#[should_panic(expected = "CKP-5")]
async fn one_slot_for_every_scope_is_reported_as_a_ckp_5_violation() {
    checkpoint::verify_deterministic(&FaultyCheckpointStore::injecting(
        CheckpointFault::OneSlotForEveryScope,
    ))
    .await;
}

/// The rendered key is the whole of what a store is told, and a framework-owned
/// consumer renders inside the reserved namespace deliberately — that is what
/// keeps its key from meeting a projection's. A store that read the namespace as
/// something to validate would keep those consumers from recording anything.
#[tokio::test]
#[should_panic(expected = "CKP-5")]
async fn refusing_a_reserved_namespace_key_is_reported_as_a_ckp_5_violation() {
    checkpoint::verify_deterministic(&FaultyCheckpointStore::injecting(
        CheckpointFault::RefuseAKeyInsideTheReservedNamespace,
    ))
    .await;
}

/// A provider that builds a store per request splits the world in two with no
/// error raised anywhere: both handles work, and a record written through one
/// is simply not there through the other.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[should_panic(expected = "PRV-1")]
async fn a_store_of_its_own_per_call_is_reported_as_a_prv_1_violation() {
    provider::verify_event_store(&FaultyProvider, &TokioExecutor).await;
}

/// Configuring both checkpoint slots with one store is allowed, so the clause
/// cannot be about the store being shared. What it is about is the two
/// consumers being told apart inside it — and a store with one position for
/// every key tells them apart nowhere.
#[tokio::test]
#[should_panic(expected = "PRV-4")]
async fn checkpoint_slots_that_cannot_be_told_apart_are_reported_as_a_prv_4_violation() {
    provider::verify_checkpoint_independence(&ProviderWithCollidingCheckpointSlots).await;
}
