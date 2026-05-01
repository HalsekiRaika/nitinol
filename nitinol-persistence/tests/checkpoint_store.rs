use nitinol_persistence::store::{CheckpointStore, InMemoryCheckpointStore};
use nitinol_persistence::ProjectionId;

/// save した checkpoint を load で取得でき、値が一致する
#[tokio::test]
async fn save_and_load_checkpoint_roundtrip() {
    // Given: an empty store and one projection
    let store = InMemoryCheckpointStore::default();
    let proj = ProjectionId::new("proj-1");

    // When: sequence 42 is saved and then loaded
    store
        .save(&proj, 42, None)
        .await
        .expect("save should succeed");
    let loaded = store
        .load(&proj)
        .await
        .expect("load should succeed");

    // Then: the saved sequence is returned
    assert_eq!(loaded, Some(42));
}

/// 同一 Projection に複数回 save した場合、最後の値で上書きされる（後勝ち）
#[tokio::test]
async fn multiple_saves_update_checkpoint_to_latest_value() {
    // Given: an empty store
    let store = InMemoryCheckpointStore::default();
    let proj = ProjectionId::new("proj-1");

    // When: three different sequence values are saved in order
    store
        .save(&proj, 10, None)
        .await
        .expect("save seq=10 should succeed");
    store
        .save(&proj, 20, None)
        .await
        .expect("save seq=20 should succeed");
    store
        .save(&proj, 30, None)
        .await
        .expect("save seq=30 should succeed");

    let loaded = store
        .load(&proj)
        .await
        .expect("load should succeed");

    // Then: the last written value (30) is returned
    assert_eq!(loaded, Some(30));
}

/// 未保存 Projection への load は Ok(None) を返す（エラーにならない）
#[tokio::test]
async fn load_for_unknown_projection_returns_none() {
    // Given: an empty store
    let store = InMemoryCheckpointStore::default();
    let proj = ProjectionId::new("nonexistent");

    // When: loading a checkpoint for a projection that has none
    let result = store
        .load(&proj)
        .await
        .expect("load should succeed even for unknown projection");

    // Then: Ok(None) is returned
    assert!(result.is_none());
}

/// tx: None を渡しても正常に動作する（InMemory は TX を使わない）
#[tokio::test]
async fn save_with_none_tx_succeeds() {
    // Given: an empty store
    let store = InMemoryCheckpointStore::default();
    let proj = ProjectionId::new("proj-1");

    // When: saving with tx=None (InMemory Tx is ())
    let result = store.save(&proj, 99, None).await;

    // Then: the call succeeds and the value is persisted
    assert!(result.is_ok(), "save with tx=None should succeed");
    let loaded = store
        .load(&proj)
        .await
        .expect("load should succeed");
    assert_eq!(loaded, Some(99));
}
