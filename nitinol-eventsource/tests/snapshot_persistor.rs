// Integration tests for SnapshotPersistor process.
//
// SnapshotPersistor is a dedicated persistence process that owns Arc<dyn SnapshotStore>.
// AggregateProcess communicates with it via SnapshotPersistorRef, never holding the
// store directly — satisfying the Phase 2 lockless messaging design.
//
// Compile errors are expected until SnapshotPersistor and SnapshotPersistorRef are
// implemented in nitinol-eventsource/src/process/persistence/.

use std::sync::Arc;

use bytes::Bytes;
use nitinol_eventsource::{SnapshotPersistor, SnapshotPersistorProxy};
use nitinol_persistence::store::InMemorySnapshotStore;
use nitinol_persistence::{AggregateId, PersistedSnapshot};
use nitinol_runtime::ProcessSystem;

// Test helpers

/// Spawns a fresh ProcessSystem with an InMemorySnapshotStore-backed SnapshotPersistor.
async fn setup() -> (ProcessSystem, SnapshotPersistorProxy) {
    let system = ProcessSystem::new().await;
    let store = Arc::new(InMemorySnapshotStore::default());
    let snapshot_ref = SnapshotPersistor::spawn(&system, store).await;
    (system, snapshot_ref)
}

fn make_snapshot(aggregate_id: &AggregateId, sequence: u64, value: u64) -> PersistedSnapshot {
    PersistedSnapshot {
        aggregate_id: aggregate_id.clone(),
        sequence,
        payload: Bytes::from(value.to_be_bytes().to_vec()),
        created_at: jiff::Timestamp::now(),
    }
}

// SaveSnapshot + LoadLatestSnapshot: the ordinary path

/// After SaveSnapshot, LoadLatestSnapshot returns that same snapshot.
#[tokio::test]
async fn snapshot_persistor_save_and_load_latest() {
    // Given
    let (_system, snapshot_ref) = setup().await;
    let id = AggregateId::new("sp-save-load");
    let snapshot = make_snapshot(&id, 5, 42);

    // When
    snapshot_ref
        .save(snapshot)
        .await
        .expect("save must succeed");
    let loaded = snapshot_ref
        .load_latest(id)
        .await
        .expect("load_latest must succeed");

    // Then
    let loaded = loaded.expect("snapshot must be present after save");
    assert_eq!(
        loaded.sequence, 5,
        "loaded sequence must match saved sequence"
    );
    assert_eq!(
        loaded.payload,
        Bytes::from(42u64.to_be_bytes().to_vec()),
        "loaded payload must match saved payload"
    );
}

// LoadLatestSnapshot: None where no snapshot exists

/// For an aggregate_id no snapshot was saved for, LoadLatestSnapshot
/// returns None.
#[tokio::test]
async fn snapshot_persistor_load_when_no_snapshot_returns_none() {
    // Given
    let (_system, snapshot_ref) = setup().await;

    // When
    let loaded = snapshot_ref
        .load_latest(AggregateId::new("sp-no-snapshot"))
        .await
        .expect("load_latest must not error");

    // Then
    assert!(
        loaded.is_none(),
        "load_latest with no saved snapshot must return None"
    );
}

// LoadLatestSnapshot: the highest sequence after several saves

/// After saving sequence=3 and sequence=5, LoadLatestSnapshot returns
/// sequence=5.
#[tokio::test]
async fn snapshot_persistor_load_latest_returns_highest_sequence() {
    // Given: save two snapshots in ascending sequence order
    let (_system, snapshot_ref) = setup().await;
    let id = AggregateId::new("sp-latest");
    snapshot_ref
        .save(make_snapshot(&id, 3, 3))
        .await
        .expect("save seq=3 must succeed");
    snapshot_ref
        .save(make_snapshot(&id, 5, 5))
        .await
        .expect("save seq=5 must succeed");

    // When
    let loaded = snapshot_ref
        .load_latest(id)
        .await
        .expect("load_latest must succeed");

    // Then: the snapshot with the highest sequence is returned
    let loaded = loaded.expect("snapshot must be present");
    assert_eq!(
        loaded.sequence, 5,
        "load_latest must return the snapshot with highest sequence"
    );
    assert_eq!(
        loaded.payload,
        Bytes::from(5u64.to_be_bytes().to_vec()),
        "payload must correspond to sequence=5"
    );
}

// LoadLatestSnapshot: snapshots of different aggregate_ids do not mix

/// Snapshots saved under two aggregate_ids are independent of each other.
#[tokio::test]
async fn snapshot_persistor_load_isolates_by_aggregate_id() {
    // Given: save snapshots for id_a (value=10, seq=2) and id_b (value=20, seq=4)
    let (_system, snapshot_ref) = setup().await;
    let id_a = AggregateId::new("sp-isolate-a");
    let id_b = AggregateId::new("sp-isolate-b");

    snapshot_ref
        .save(make_snapshot(&id_a, 2, 10))
        .await
        .expect("save id_a must succeed");
    snapshot_ref
        .save(make_snapshot(&id_b, 4, 20))
        .await
        .expect("save id_b must succeed");

    // When
    let loaded_a = snapshot_ref
        .load_latest(id_a.clone())
        .await
        .expect("load_latest id_a must succeed");
    let loaded_b = snapshot_ref
        .load_latest(id_b.clone())
        .await
        .expect("load_latest id_b must succeed");

    // Then: each returns its own snapshot, unaffected by the other
    let snap_a = loaded_a.expect("id_a snapshot must be present");
    let snap_b = loaded_b.expect("id_b snapshot must be present");

    assert_eq!(
        snap_a.aggregate_id, id_a,
        "id_a snapshot must have correct aggregate_id"
    );
    assert_eq!(snap_a.sequence, 2, "id_a snapshot must have sequence=2");
    assert_eq!(
        snap_b.aggregate_id, id_b,
        "id_b snapshot must have correct aggregate_id"
    );
    assert_eq!(snap_b.sequence, 4, "id_b snapshot must have sequence=4");
}

// SaveSnapshot: load_latest returns the newest after an overwrite

/// Saving over the same aggregate_id with a higher sequence makes the next
/// LoadLatestSnapshot return the newer value.
#[tokio::test]
async fn snapshot_persistor_overwrite_returns_newer_snapshot() {
    // Given: save an initial snapshot, then overwrite with a newer one
    let (_system, snapshot_ref) = setup().await;
    let id = AggregateId::new("sp-overwrite");

    snapshot_ref
        .save(make_snapshot(&id, 1, 100))
        .await
        .expect("initial save must succeed");
    snapshot_ref
        .save(make_snapshot(&id, 10, 999))
        .await
        .expect("overwrite save must succeed");

    // When
    let loaded = snapshot_ref
        .load_latest(id)
        .await
        .expect("load_latest must succeed");

    // Then: the newer snapshot (seq=10) is returned
    let snap = loaded.expect("snapshot must be present");
    assert_eq!(
        snap.sequence, 10,
        "load_latest must return the newest snapshot"
    );
    assert_eq!(
        snap.payload,
        Bytes::from(999u64.to_be_bytes().to_vec()),
        "payload must correspond to the newest snapshot"
    );
}
