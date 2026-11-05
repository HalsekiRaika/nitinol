//! The store a deployment's store erases to.

use crate::provider::{sealed, SnapshotsForRecovery, Store};
use crate::store::{Album, Journal};

/// A store with its type erased: the journal, and the snapshot slot as a
/// value.
///
/// Wiring whose types are fixed before a deployment's store is known names
/// this one type for every store. What the type of the original store said
/// about its slots is gone, so whether it held a snapshot slot is carried as an
/// `Option` and read when the program runs.
///
/// # Why it implements no slot trait
///
/// The slot traits are what wiring compiled against a store reads. Implementing
/// [`SnapshotSlot`](crate::provider::SnapshotSlot) here would hand wiring that
/// needs one a slot it could not tell was missing, so the snapshot slot is read
/// through [`Store::snapshot_slot`] alone and wiring that takes a slot's bound
/// still refuses this store when it is compiled — which is `PRV-3`. The offset
/// slots are not carried at all: what reaches a store through an erased handle
/// is an aggregate, and an aggregate keeps no offset.
///
/// # A handle onto the store it was taken from
///
/// Both halves are the original store's own clones, so a fact appended and a
/// snapshot saved through this store, or through any clone of it, reach the
/// records the original reaches (`PRV-1`).
#[derive(Clone, Debug)]
pub struct ErasedStore {
    events: Journal,
    snapshots: Option<Album>,
}

impl ErasedStore {
    /// Erase the type of `store`, keeping its journal and whether it holds a
    /// snapshot slot.
    ///
    /// Read through [`Store`] alone, so this is the one way the two are taken
    /// out of a store, and erasing an erased store keeps the slot it carries.
    pub fn of<St: Store>(store: &St) -> Self {
        Self {
            events: store.events(),
            snapshots: store.snapshot_slot(),
        }
    }
}

impl Store for ErasedStore {
    fn events(&self) -> Journal {
        self.events.clone()
    }

    fn snapshot_slot(&self) -> Option<Album> {
        self.snapshots.clone()
    }
}

impl SnapshotsForRecovery for ErasedStore {
    fn snapshots_for_recovery(&self) -> Option<Album> {
        self.snapshot_slot()
    }
}

impl sealed::Sealed for ErasedStore {}
