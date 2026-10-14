//! Where the stores a deployment runs on come from.
//!
//! Three responsibilities, kept apart.
//!
//! | Layer | Responsibility |
//! |---|---|
//! | [`Store`] | Owns the storage resources and the identity of the regions behind them |
//! | [`CustomStore`] | Names one handle per slot, which is how a mixture is assembled |
//! | [`PersistenceProvider`] | Distributes what a store answers with to the framework's wiring |
//!
//! A deployment constructs a store — [`InMemoryStore`](crate::store::InMemoryStore),
//! a backend's own, or a [`CustomStore`] over named handles — and hands it to
//! [`PersistenceProvider::new`]. Every wiring point then resolves its slot from
//! there, so an operator names each logical destination once and nothing
//! reaches a store of its own.
//!
//! The clauses a store owes are stated on [`Store`], and
//! `nitinol-store-conformance`'s `provider` module is the suite one is put
//! through.

mod custom_store;

use crate::store::{Album, Journal, OffsetStore};

pub use self::custom_store::{CustomStore, CustomStoreBuilder, EventStoreUnset, Unprovided};

/// The storage a deployment runs on, as one cloneable handle.
///
/// A store is not a factory. It answers for the resources it *holds*, so a slot
/// is a logical destination — a name for a region of data — rather than a
/// constructor the caller happens to reach through. The slots beyond the
/// journal are the sub-traits below, and a store that supplies none of them
/// implements this trait alone.
///
/// # Why the journal is not a slot of its own
///
/// There is no event-sourced deployment without events, so every store owes
/// one. Making it optional would put a store that supplies nothing at all in
/// the type system with nothing to do.
///
/// # Why the handle is `Clone`
///
/// A store is distributed by being cloned: into a
/// [`PersistenceProvider`], into whatever wiring holds one, and into every slot
/// handle it answers with. Identity is therefore the store's own obligation
/// rather than something a caller establishes by wrapping it in an `Arc` —
/// which is what `PRV-1` below says. How the sharing is done inside is the
/// backend's: in-memory state behind an `Arc`, a connection pool and a schema
/// copied, a client handle cloned.
///
/// The clauses below are the contract every store must reproduce. They are what
/// wiring and a third-party assembly may rely on, and each carries a stable
/// label (`PRV-1` … `PRV-5`).
///
/// # PRV-1: distribution and duplication are idempotent
///
/// For every slot a store supplies, handles obtained **any** of three ways
/// reach one set of records: taking the slot twice from one store, taking it
/// from a clone of the store handle, and taking it from a
/// [`PersistenceProvider`] over either.
///
/// This is what makes a store a single destination rather than a convenience.
/// A slot answering with a region of its own per call — or a `Clone` that
/// builds fresh state — hands back two working handles onto two worlds, and
/// nothing reports it: an aggregate's append and a projector's load simply
/// never meet.
///
/// Fixed by the suite's per-slot `PRV-1` clauses, which read across a repeated
/// take and across a clone, and by
/// `nitinol-persistence/tests/persistence_provider.rs`.
///
/// # PRV-2: identity is of the data region, not of the connection
///
/// Two handles are one slot when they reach the same records. Sharing a
/// connection pool is neither necessary nor sufficient for that: one pool over
/// two schemas is two regions, and two pools over one schema is one region.
///
/// A store assembling handles over a backend therefore owes the region rather
/// than the plumbing — one offset handle named into both slots, or two
/// pools onto one schema, are both conforming, and one pool onto two schemas is
/// not. PRV-1 is how the region is observed: a record written through one
/// handle and read through another.
///
/// The pool half of this is the implementor's obligation and is not fixed by a
/// test. A suite cannot see what a handle is connected through, only what it
/// answers with, which is what PRV-1 reads.
///
/// # PRV-3: a mixture is assembled explicitly or not at all
///
/// A store answers for its own backend's resources. A slot it has no store for
/// is one it does not implement the sub-trait of, so the wiring that needs it
/// does not compile — rather than being filled quietly from somewhere else.
///
/// Mixing backends is therefore something an operator writes down: a
/// [`CustomStore`] naming each handle in turn, one of which comes from
/// elsewhere.
///
/// Fixed by the `compile_fail` doctests on [`CustomStoreBuilder`] and on the
/// wiring entry points in `nitinol-eventsource` and `nitinol-saga`.
///
/// # PRV-4: the user's read side and the framework's read side are two slots
///
/// [`ProjectionOffsetSlot`] and [`SagaOffsetSlot`] are configured,
/// initialised and recovered independently: writing or reinitialising one
/// leaves the other's answer where it was.
///
/// What the two slots are for is a deployment choice rather than a kind of
/// consumer. A projection's offset belongs beside the read model it is building,
/// which is the database an operator chose for *their* read side; the offsets of
/// the framework's own attachments belong beside the journal, which is the
/// framework's. Two slots is how an operator can put each where it belongs, and
/// `PRV-3` is why neither is filled from the other.
///
/// Naming both with one handle is allowed and is how a single-table backend is
/// deployed. What keeps the two apart there is the key each consumer renders —
/// a user-named scope verbatim, a framework-owned one inside the
/// [reserved namespace](crate::reserved) — which is the offset contract's
/// `OFS-5`. What this clause adds is that the store does not fold the two slots
/// into one answer, so replacing the handle one slot is named with does not
/// reach the other.
///
/// Fixed by the suite's `verify_offset_independence`, and by
/// `nitinol-saga/tests/persistence_projection_rebuild_leaves_delivery_progress.rs`
/// for the rebuild a running deployment performs.
///
/// # PRV-5: a supplier is not a transaction
///
/// One store is not one atomic unit. There is no transaction spanning the
/// journal, the snapshots and the offsets, and none spanning the two
/// offset slots: a write taken by one slot stands whether or not the next
/// one is taken.
///
/// The atomic units are stated elsewhere and are the only ones there are.
/// [`EventStore::append`](crate::store::EventStore::append) decides a stream's
/// own `OCC-3`, and a projection's result settling together with the offset
/// covering it is [`OffsetStore`]'s `OFS-6` driven by a projector's
/// transaction provider. Neither becomes wider because one store hands out both
/// resources.
///
/// **Not fixed by a test.** The absence of atomicity across destinations has no
/// single deterministic observation: a suite judging it would have to construct
/// a crash between two writes in every backend. It is stated here because the
/// name invites the opposite reading.
pub trait Store: Clone + Send + Sync + 'static {
    /// The journal every stream in this deployment is appended to and replayed
    /// from.
    ///
    /// What comes back is the registered store's own clone with its type
    /// erased, which is why there is no associated type here: the backend is
    /// named once, where the store is assembled, and every wiring point reads
    /// one handle type.
    fn events(&self) -> Journal;
}

/// A store that also holds somewhere to keep aggregate snapshots.
///
/// Wiring that needs snapshots takes this bound, so a store without the slot is
/// refused while the wiring is compiled rather than when a save is first
/// attempted.
pub trait SnapshotSlot: Store {
    /// Where aggregate snapshots are kept.
    ///
    /// Erased for the reason [`Store::events`] is.
    fn snapshots(&self) -> Album;
}

/// A store that also holds somewhere to keep how far a projection has been
/// applied.
///
/// The handle is an associated type rather than a `dyn` one because
/// [`OffsetStore::Tx`] is an associated type of its own: a projector
/// settling its read-model update and its offset in one transaction needs
/// the backend's transaction type to be visible, which erasing the store would
/// take away.
pub trait ProjectionOffsetSlot: Store {
    /// The backend's own offset store, as the projection slot is named with
    /// it.
    type ProjectionOffsets: OffsetStore + Clone + 'static;

    /// Where a projection's applied position is kept.
    fn projection_offsets(&self) -> Self::ProjectionOffsets;
}

/// A store that also holds somewhere to keep how far the framework's own saga
/// attachments have got.
///
/// This is the slot for **every** attachment a saga runs with, not the delivery
/// host's alone: the host records what it has delivered here, and so do the
/// manager and the deadline index as they are written. What they have in common
/// is that the framework owns them, so their offsets belong wherever an operator
/// put the journal — which is the axis `PRV-4` states.
///
/// A distinct associated type name from
/// [`ProjectionOffsetSlot::ProjectionOffsets`] so that a store
/// supplying both slots can be named in a `where` clause without the two
/// projections of it becoming ambiguous.
pub trait SagaOffsetSlot: Store {
    /// The backend's own offset store, as the saga slot is named with it.
    type SagaOffsets: OffsetStore + Clone + 'static;

    /// Where the framework's saga attachments keep their progress.
    fn saga_offsets(&self) -> Self::SagaOffsets;
}

/// The supplier every event-sourced deployment has one of.
///
/// It distributes what the [`Store`] it was constructed over answers with, in
/// the shape the framework's wiring takes: the journal as a [`Journal`] and the
/// snapshot store as an [`Album`] — the erased handles the wiring is written
/// against — and each offset slot as the backend's own type so a transaction
/// stays visible.
///
/// Every one of them is the store's own handle, answered by value. Nothing here
/// shares a handle on a caller's behalf: identity is the store's obligation
/// (`PRV-1`), and a supplier that wrapped a handle anyway would be proving the
/// same property a second time.
///
/// What it does not do is decide anything. The identity of a slot, and whether
/// a slot exists at all, belong to the store — which is why a supplier adds no
/// clause of its own and the contract is stated on [`Store`].
///
/// Cloning a supplier names the same store, so every consumer wired from one
/// clone and every consumer wired from another reach one set of records.
#[derive(Clone)]
pub struct PersistenceProvider<S> {
    store: S,
}

impl<S: Store> PersistenceProvider<S> {
    /// Distribute the slots `store` holds.
    pub fn new(store: S) -> Self {
        Self { store }
    }

    /// The store this supplier distributes.
    pub fn store(&self) -> &S {
        &self.store
    }

    /// The journal every stream in this deployment is appended to and replayed
    /// from.
    pub fn event_store(&self) -> Journal {
        self.store.events()
    }
}

impl<S: SnapshotSlot> PersistenceProvider<S> {
    /// Where aggregate snapshots are kept.
    pub fn snapshot_store(&self) -> Album {
        self.store.snapshots()
    }
}

impl<S: ProjectionOffsetSlot> PersistenceProvider<S> {
    /// Where a projection's applied position is kept.
    pub fn projection_offsets(&self) -> S::ProjectionOffsets {
        self.store.projection_offsets()
    }
}

impl<S: SagaOffsetSlot> PersistenceProvider<S> {
    /// Where the framework's saga attachments keep their progress.
    pub fn saga_offsets(&self) -> S::SagaOffsets {
        self.store.saga_offsets()
    }
}
