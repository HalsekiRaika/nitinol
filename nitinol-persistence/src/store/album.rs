//! The snapshot store a store answers with, once the backend's type is out of
//! the signature.

use std::fmt;

use crate::error::SnapshotError;
use crate::id::AggregateId;
use crate::snapshot::PersistedSnapshot;
use crate::store::snapshot_store::SnapshotStore;

/// The snapshot store registered with the framework, with the backend's type
/// erased.
///
/// [`Journal`](crate::store::Journal)'s counterpart, and the same thing in the
/// snapshot half: what is inside is the registered store's own clone, and
/// [`Clone`] here is that backend's clone one level down — through the
/// [`SnapshotStoreClone`](crate::store::SnapshotStoreClone) supertrait — so
/// every wiring point handed an album reaches the one set of saves.
///
/// # Why it does not implement `SnapshotStore`
///
/// The same two reasons the journal's rustdoc states. `impl<S: SnapshotStore>
/// From<S> for Album` has room beside `core`'s reflexive conversion only
/// because an album is not a snapshot store, and erasing an album a second time
/// is a type error rather than a `Box<Box<_>>`.
///
/// ```compile_fail
/// use nitinol_persistence::store::{Album, InMemorySnapshotStore, SnapshotStore};
///
/// fn takes(_: &impl SnapshotStore) {}
///
/// let album = Album::from(InMemorySnapshotStore::default());
/// // an album is a handle onto a snapshot store, not a snapshot store
/// takes(&album);
/// ```
///
/// ```compile_fail
/// use nitinol_persistence::store::{Album, InMemorySnapshotStore};
///
/// let album = Album::from(InMemorySnapshotStore::default());
/// // erasing an erased handle: there is no second box to put it in
/// let _twice = Album::from(Box::new(album));
/// ```
///
/// # Why it does not implement `Store`
///
/// There is no deployment whose storage is snapshots alone — a snapshot is a
/// shortcut and never a record — so unlike a journal, an album is not a
/// [`Store`](crate::provider::Store) in its own right.
pub struct Album(Box<dyn SnapshotStore>);

impl Album {
    /// Stores `snapshot` as the state of its aggregate at its `sequence`.
    ///
    /// [`SnapshotStore::save`]'s clauses, unchanged: this hands the call to the
    /// registered store and its answer back.
    pub async fn save(&self, snapshot: PersistedSnapshot) -> Result<(), SnapshotError> {
        self.0.save(snapshot).await
    }

    /// The snapshot `aggregate_id` saved last, or `Ok(None)` where it has saved
    /// none.
    ///
    /// [`SnapshotStore::load_latest`]'s clauses, unchanged.
    pub async fn load_latest(
        &self,
        aggregate_id: &AggregateId,
    ) -> Result<Option<PersistedSnapshot>, SnapshotError> {
        self.0.load_latest(aggregate_id).await
    }
}

impl Clone for Album {
    fn clone(&self) -> Self {
        Self(self.0.clone_box())
    }
}

impl fmt::Debug for Album {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Album").finish_non_exhaustive()
    }
}

impl<S: SnapshotStore + 'static> From<S> for Album {
    fn from(store: S) -> Self {
        Self(Box::new(store))
    }
}

/// Taking an album from a reference to one is cloning it.
impl From<&Album> for Album {
    fn from(album: &Album) -> Self {
        album.clone()
    }
}
