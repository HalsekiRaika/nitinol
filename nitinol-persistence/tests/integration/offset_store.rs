//! Offset contract tests for [`InMemoryOffsetStore`].
//!
//! These tests are the record of the offset behaviour every
//! `OffsetStore` backend must reproduce, as the reference implementation
//! keeps it.  What binds a backend is the contract on [`OffsetStore`]; what
//! a backend of its own is put through is `nitinol-store-conformance`, which
//! runs these same labels against any store.  Each contract carries a stable
//! label, which the doc of the test fixing it here begins with:
//!
//! | Label | Contract |
//! |---|---|
//! | `OFS-1` | Never started is not the same as at zero: `load` for a scope nothing has saved a position for answers `Ok(None)`, and not `Ok(Some(0))`. |
//! | `OFS-2` | A saved position is what a load answers with. |
//! | `OFS-3` | A move forward is taken however far it reaches: a position beyond the one the store holds is taken whether or not it is the next number, and `load` then answers with it. |
//! | `OFS-6` | Where `Tx` is `()` there is nothing for a save to join, and the save stands on its own.  This store is such a backend; the half where `Tx` is a real transaction is the Postgres provider's to fix. |
//!
//! Every save in this file carries the position forward.  `OFS-4` — what a save
//! that is not a move forward answers — is fixed by no test here and by none in
//! the suite: refusing such a save and taking it are both conforming, so there
//! is no observation to ask every backend for.  The contract on
//! [`OffsetStore`] says why, and a backend that refuses owes a test of its
//! own.
//!
//! `OFS-5` — that the rendered key is the whole of what a store is told — has no
//! test in this file, where every test holds a single key. It is fixed by the
//! `offset` module of `nitinol-store-conformance`, deterministic half and
//! concurrent half both, for this store and for any other.

use nitinol_persistence::store::{InMemoryOffsetStore, OffsetScope, OffsetStore};

/// OFS-2: A saved offset is returned by load, with the same value.
#[tokio::test]
async fn save_and_load_offset_roundtrip() {
    // Given: an empty store and one projection
    let store = InMemoryOffsetStore::default();
    let proj = OffsetScope::new("proj-1");

    // When: sequence 42 is saved and then loaded
    store
        .save(&proj, 42, None)
        .await
        .expect("save should succeed");
    let loaded = store.load(&proj).await.expect("load should succeed");

    // Then: the saved sequence is returned
    assert_eq!(loaded, Some(42));
}

/// OFS-3: Several saves against one Projection leave the last value (last
/// writer wins).
#[tokio::test]
async fn multiple_saves_update_offset_to_latest_value() {
    // Given: an empty store
    let store = InMemoryOffsetStore::default();
    let proj = OffsetScope::new("proj-1");

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

    let loaded = store.load(&proj).await.expect("load should succeed");

    // Then: the last written value (30) is returned
    assert_eq!(loaded, Some(30));
}

/// OFS-1: load for an unsaved Projection returns Ok(None), and is not an error.
#[tokio::test]
async fn load_for_unknown_projection_returns_none() {
    // Given: an empty store
    let store = InMemoryOffsetStore::default();
    let proj = OffsetScope::new("nonexistent");

    // When: loading an offset for a projection that has none
    let result = store
        .load(&proj)
        .await
        .expect("load should succeed even for unknown projection");

    // Then: Ok(None) is returned
    assert!(result.is_none());
}

/// OFS-6: Handing tx: None works as usual — InMemory uses no transaction.
#[tokio::test]
async fn save_with_none_tx_succeeds() {
    // Given: an empty store
    let store = InMemoryOffsetStore::default();
    let proj = OffsetScope::new("proj-1");

    // When: saving with tx=None (InMemory Tx is ())
    let result = store.save(&proj, 99, None).await;

    // Then: the call succeeds and the value is persisted
    assert!(result.is_ok(), "save with tx=None should succeed");
    let loaded = store.load(&proj).await.expect("load should succeed");
    assert_eq!(loaded, Some(99));
}
