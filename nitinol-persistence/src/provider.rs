//! Where the stores a deployment runs on come from.
//!
//! One supplier hands out the [`EventStore`](crate::store::EventStore), the
//! [`SnapshotStore`](crate::store::SnapshotStore) and the checkpoint stores the
//! framework's wiring needs, so an operator names each logical destination once
//! and every consumer resolves it from there. Handing the same store to two
//! wiring points by hand is what splits a deployment into two worlds, and
//! nothing but the operator's attention stood in the way of it.
//!
//! The traits below are the contract, and [`Persistence`] is the assembly a
//! deployment names its handles in.

mod persistence;

use std::sync::Arc;

use crate::store::{CheckpointStore, EventStore, SnapshotStore};

pub use self::persistence::{
    DeliveryCheckpointsProvided, InMemoryPersistence, Persistence, ProjectionCheckpointsProvided,
    SnapshotStoreProvided, Unprovided,
};

/// The supplier every event-sourced deployment has one of: where the journal
/// comes from.
///
/// A provider is not a factory. It answers for the stores it was *configured
/// with*, so the slot is a logical destination — a name for a region of data —
/// rather than a constructor the caller happens to reach through. The slots
/// beyond the journal are the sub-traits below, and a provider that supplies
/// none of them implements this trait alone.
///
/// # Why the journal is not a slot of its own
///
/// There is no event-sourced deployment without events, so every provider owes
/// one. Making it optional would put a provider that supplies nothing at all in
/// the type system with nothing to do.
///
/// The clauses below are the contract every provider must reproduce. They are
/// what wiring and a third-party assembly may rely on, and each carries a
/// stable label (`PRV-1` … `PRV-5`). `nitinol-store-conformance`'s `provider`
/// module is the suite a provider is put through.
///
/// # PRV-1: distribution is idempotent
///
/// For every slot a provider supplies, taking a handle twice reaches one set of
/// records: what is written through one handle is read through the other.
///
/// This is what makes a provider a single supplier rather than a convenience.
/// A slot answering with a store of its own per call hands back two working
/// handles onto two worlds, and nothing reports it — an aggregate's append and
/// a projector's load simply never meet.
///
/// Fixed by the suite's per-slot `PRV-1` clauses, and by
/// `nitinol-persistence/tests/persistence_provider.rs` against [`Persistence`].
///
/// # PRV-2: identity is of the data region, not of the connection
///
/// Two handles are one slot when they reach the same records. Sharing a
/// connection pool is neither necessary nor sufficient for that: one pool over
/// two schemas is two regions, and two pools over one schema is one region.
///
/// A provider assembling handles over a backend therefore owes the region
/// rather than the plumbing — `with_checkpoints` on one store, or two pools
/// onto one schema, are both conforming, and one pool onto two schemas is not.
/// PRV-1 is how the region is observed: a record written through one handle and
/// read through another.
///
/// The pool half of this is the assembling implementor's obligation and is not
/// fixed by a test. A suite cannot see what a handle is connected through, only
/// what it answers with, which is what PRV-1 reads.
///
/// # PRV-3: a mixture is assembled explicitly or not at all
///
/// A provider answers for its own backend's stores. A slot it has no store for
/// is one it does not implement the sub-trait of, so the wiring that needs it
/// does not compile — rather than being filled quietly from somewhere else.
///
/// Mixing backends is therefore something an operator writes down: a
/// [`Persistence`] naming each handle in turn, one of which comes from
/// elsewhere.
///
/// Fixed by the `compile_fail` doctests on the wiring entry points in
/// `nitinol-eventsource` and `nitinol-saga`.
///
/// # PRV-4: a projection's position and a delivery host's progress are two slots
///
/// [`ProjectionCheckpointProvider`] and [`DeliveryCheckpointProvider`] are
/// configured, initialised and recovered independently: writing or
/// reinitialising one leaves the other's answer where it was.
///
/// Configuring both with one store is allowed and is how a single-table backend
/// is deployed. What keeps the two apart there is the key each consumer renders
/// — a projection verbatim, a framework-owned consumer inside the
/// [reserved namespace](crate::reserved) — which is the checkpoint contract's
/// `CKP-5`. What this clause adds is that the provider does not fold the two
/// slots into one answer, so replacing the store one slot is configured with
/// does not reach the other.
///
/// Fixed by the suite's `verify_checkpoint_independence`, and by
/// `nitinol-saga/tests/persistence_projection_rebuild_leaves_delivery_progress.rs`
/// for the rebuild a running deployment performs.
///
/// # PRV-5: a provider is not a transaction
///
/// One supplier is not one atomic unit. There is no transaction spanning the
/// journal, the snapshots and the checkpoints, and none spanning the two
/// checkpoint slots: a write taken by one slot stands whether or not the next
/// one is taken.
///
/// The atomic units are stated elsewhere and are the only ones there are.
/// [`EventStore::append`](crate::store::EventStore::append) decides a stream's
/// own `OCC-3`, and a projection's result settling together with the checkpoint
/// covering it is [`CheckpointStore`]'s `CKP-6` driven by a projector's
/// transaction provider. Neither becomes wider because one provider hands out
/// both stores.
///
/// **Not fixed by a test.** The absence of atomicity across destinations has no
/// single deterministic observation: a suite judging it would have to construct
/// a crash between two writes in every backend. It is stated here because the
/// name invites the opposite reading.
pub trait PersistenceProvider: Send + Sync + 'static {
    /// The journal every stream in this deployment is appended to and replayed
    /// from.
    fn event_store(&self) -> Arc<dyn EventStore>;
}

/// A provider that also supplies somewhere to keep aggregate snapshots.
///
/// Wiring that needs snapshots takes this bound, so a provider without the slot
/// is refused while the wiring is compiled rather than when a save is first
/// attempted.
pub trait SnapshotStoreProvider: PersistenceProvider {
    /// Where aggregate snapshots are kept.
    fn snapshot_store(&self) -> Arc<dyn SnapshotStore>;
}

/// A provider that also supplies somewhere to keep how far a projection has
/// been applied.
///
/// The store is an associated type rather than a `dyn` handle because
/// [`CheckpointStore::Tx`] is an associated type of its own: a projector
/// settling its read-model update and its checkpoint in one transaction needs
/// the backend's transaction type to be visible, which erasing the store would
/// take away.
pub trait ProjectionCheckpointProvider: PersistenceProvider {
    /// The backend's own checkpoint store, as the projection slot is configured
    /// with it.
    type ProjectionCheckpoints: CheckpointStore + 'static;

    /// Where a projection's applied position is kept.
    fn projection_checkpoints(&self) -> Arc<Self::ProjectionCheckpoints>;
}

/// A provider that also supplies somewhere to keep how far a delivery host has
/// got.
///
/// A distinct associated type name from
/// [`ProjectionCheckpointProvider::ProjectionCheckpoints`] so that a provider
/// supplying both slots can be named in a `where` clause without the two
/// projections of it becoming ambiguous.
pub trait DeliveryCheckpointProvider: PersistenceProvider {
    /// The backend's own checkpoint store, as the delivery slot is configured
    /// with it.
    type DeliveryCheckpoints: CheckpointStore + 'static;

    /// Where a delivery host's progress is kept.
    fn delivery_checkpoints(&self) -> Arc<Self::DeliveryCheckpoints>;
}

/// A provider reached through an `Arc` is the same provider.
///
/// What this is for is wiring that has to name one type for every provider: a
/// delivery host fixes the system type its dispatch closures take when the
/// mapping is written, long before the provider a deployment assembles is
/// known. Erasing to `Arc<dyn PersistenceProvider>` is how it names one, and
/// this implementation is what makes the erased handle a provider in its own
/// right.
impl<T: PersistenceProvider + ?Sized> PersistenceProvider for Arc<T> {
    fn event_store(&self) -> Arc<dyn EventStore> {
        T::event_store(self)
    }
}
