use bytes::Bytes;
use jiff::Timestamp;
use nitinol_persistence::store::{InMemorySnapshotStore, SnapshotStore};
use nitinol_persistence::{AggregateId, PersistedSnapshot};

fn make_snapshot(
    aggregate_id: AggregateId,
    sequence: u64,
    payload: &'static [u8],
) -> PersistedSnapshot {
    PersistedSnapshot {
        aggregate_id,
        sequence,
        payload: Bytes::from_static(payload),
        created_at: Timestamp::now(),
    }
}

/// A saved Snapshot comes back from load_latest with the same contents.
#[tokio::test]
async fn save_and_load_latest_roundtrip() {
    // Given: an empty store and one snapshot to save
    let store = InMemorySnapshotStore::default();
    let agg = AggregateId::new("agg-1");
    let snapshot = make_snapshot(agg.clone(), 42, b"state-at-42");

    // When: the snapshot is saved and then loaded
    store.save(snapshot).await.expect("save should succeed");
    let loaded = store
        .load_latest(&agg)
        .await
        .expect("load_latest should succeed");

    // Then: the loaded snapshot matches what was saved
    let loaded = loaded.expect("snapshot should be present");
    assert_eq!(loaded.aggregate_id, agg);
    assert_eq!(loaded.sequence, 42);
    assert_eq!(loaded.payload, Bytes::from_static(b"state-at-42"));
}

/// Several saves against one Aggregate return the one saved last.
#[tokio::test]
async fn multiple_saves_return_latest_snapshot() {
    // Given: an empty store
    let store = InMemorySnapshotStore::default();
    let agg = AggregateId::new("agg-1");

    // When: three snapshots are saved in order for the same aggregate
    store
        .save(make_snapshot(agg.clone(), 1, b"state-1"))
        .await
        .expect("save seq=1 should succeed");
    store
        .save(make_snapshot(agg.clone(), 5, b"state-5"))
        .await
        .expect("save seq=5 should succeed");
    store
        .save(make_snapshot(agg.clone(), 10, b"state-10"))
        .await
        .expect("save seq=10 should succeed");

    let loaded = store
        .load_latest(&agg)
        .await
        .expect("load_latest should succeed");

    // Then: the most recently saved snapshot (seq=10) is returned
    let loaded = loaded.expect("snapshot should be present");
    assert_eq!(loaded.sequence, 10);
    assert_eq!(loaded.payload, Bytes::from_static(b"state-10"));
}

/// load_latest for an unsaved Aggregate returns Ok(None), and is not an error.
#[tokio::test]
async fn load_latest_for_unknown_aggregate_returns_none() {
    // Given: an empty store
    let store = InMemorySnapshotStore::default();
    let agg = AggregateId::new("nonexistent");

    // When: loading the latest snapshot for an aggregate that has none
    let result = store
        .load_latest(&agg)
        .await
        .expect("load_latest should succeed even for unknown aggregate");

    // Then: Ok(None) is returned
    assert!(result.is_none());
}
