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
/// - It is [`Journal`](crate::store::Journal)'s counterpart in the snapshot half
///   and has the same properties, with [`SnapshotStore`] and
///   [`SnapshotStoreClone`](crate::store::SnapshotStoreClone) in place of the
///   event-side traits.
/// - It is not a [`Store`](crate::provider::Store): a snapshot is a shortcut and
///   never a record, so no deployment's storage is snapshots alone.
///
/// An album is a handle onto a snapshot store, not a snapshot store:
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
pub struct Album(Box<dyn SnapshotStore>);

impl Album {
    /// Stores `snapshot` as the state of its aggregate at its `sequence`.
    ///
    /// [`SnapshotStore::save`]'s clauses, unchanged: this hands the call to the
    /// registered store and its answer back.
    ///
    /// # Errors
    ///
    /// Returns [`SnapshotError::Backend`] if the registered store fails to save
    /// the snapshot.
    pub async fn save(&self, snapshot: PersistedSnapshot) -> Result<(), SnapshotError> {
        self.0.save(snapshot).await
    }

    /// The snapshot `aggregate_id` saved last, or `Ok(None)` where it has saved
    /// none.
    ///
    /// [`SnapshotStore::load_latest`]'s clauses, unchanged.
    ///
    /// # Errors
    ///
    /// Returns [`SnapshotError::Backend`] if the registered store fails to read
    /// the aggregate's snapshot.
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

#[cfg(doctest)]
mod compile_fail_fixtures {
    /// ```compile_fail
    /// use nitinol_persistence::store::{Album, InMemorySnapshotStore};
    ///
    /// let album = Album::from(InMemorySnapshotStore::default());
    /// // erasing an erased handle: there is no second box to put it in
    /// let _twice = Album::from(Box::new(album));
    /// ```
    fn album_cannot_be_erased_twice() {}
}
