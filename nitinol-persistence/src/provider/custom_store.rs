//! A store assembled from handles the caller names.
//!
//! A backend may implement [`Store`] itself — the traits are public and
//! [`InMemoryStore`](crate::store::InMemoryStore) is one — but mixing backends,
//! or naming a handle a deployment already holds, is what [`CustomStore`] is
//! for: each builder call names one handle and moves that slot out of
//! [`Unprovided`], so which slots a store supplies is settled in the type.

use crate::provider::{ProjectionOffsetSlot, SagaOffsetSlot, SnapshotSlot, Store};
use crate::store::{Album, EventStore, Journal, OffsetStore, SnapshotStore};

/// A slot no handle has been named for.
///
/// The wiring that needs it does not exist on a store in this state, so "nobody
/// named a snapshot store" is a compile error at the wiring rather than a
/// failure at the first save.
#[derive(Clone, Copy, Default)]
pub struct Unprovided;

/// The journal, before one has been named.
///
/// Distinct from [`Unprovided`] because the journal is not optional: a builder
/// in this state has no [`build`](CustomStoreBuilder::build) at all, rather
/// than a store with one slot missing.
#[derive(Clone, Copy, Default)]
pub struct EventStoreUnset;

/// A [`Store`] assembled from named handles.
///
/// Each type parameter records whether its slot was named — [`Unprovided`]
/// where none was — and nothing fills a slot that was not named, which is
/// `PRV-3`: a mixture is written down or it does not happen.
///
/// There is no parameter for the journal, and the snapshot slot's is
/// [`Album`] rather than the backend it was named with. Those two are erased
/// where the handle is named, so the backend's type is written once, in the
/// builder call, rather than carried through every signature that mentions the
/// assembled store. The offset slots keep the backend's own type because
/// [`OffsetStore::Tx`] has to stay visible to a projector settling its update
/// and its position together.
///
/// Cloning this clones each handle it holds, so it reaches the regions those
/// handles name — never regions of its own.
#[derive(Clone)]
pub struct CustomStore<S = Unprovided, P = Unprovided, Sg = Unprovided> {
    events: Journal,
    snapshots: S,
    projection_offsets: P,
    saga_offsets: Sg,
}

impl CustomStore {
    /// Begin assembling a store, with no handle named yet.
    pub fn builder() -> CustomStoreBuilder {
        CustomStoreBuilder {
            events: EventStoreUnset,
            snapshots: Unprovided,
            projection_offsets: Unprovided,
            saga_offsets: Unprovided,
        }
    }
}

/// Where a [`CustomStore`]'s handles are named.
///
/// A setter exists only while its slot is unnamed, so naming one twice is a
/// compile error and the handle a slot answers with is settled in one place:
///
/// ```compile_fail
/// # use nitinol_persistence::provider::CustomStore;
/// # use nitinol_persistence::store::{InMemoryOffsetStore, InMemoryEventStore};
/// // the projection slot has already moved, so there is no second
/// // `projection_offsets`
/// CustomStore::builder()
///     .eventstore(InMemoryEventStore::default())
///     .projection_offsets(InMemoryOffsetStore::default())
///     .projection_offsets(InMemoryOffsetStore::default())
///     .build();
/// ```
///
/// And a store with no journal has no `build`:
///
/// ```compile_fail
/// # use nitinol_persistence::provider::CustomStore;
/// # use nitinol_persistence::store::InMemorySnapshotStore;
/// // no `eventstore` — there is no event-sourced deployment without events
/// CustomStore::builder()
///     .snapshot(InMemorySnapshotStore::default())
///     .build();
/// ```
pub struct CustomStoreBuilder<E = EventStoreUnset, S = Unprovided, P = Unprovided, Sg = Unprovided>
{
    events: E,
    snapshots: S,
    projection_offsets: P,
    saga_offsets: Sg,
}

impl<S, P, Sg> CustomStoreBuilder<EventStoreUnset, S, P, Sg> {
    /// Name `events` as the journal this deployment's streams are kept in.
    ///
    /// The backend is handed over as itself; it is erased here, so this is the
    /// one place its type is written. `Clone` is not named because
    /// [`EventStore`] requires it — a backend that could not be cloned could
    /// not be distributed.
    pub fn eventstore<E>(self, events: E) -> CustomStoreBuilder<Journal, S, P, Sg>
    where
        E: EventStore + 'static,
    {
        CustomStoreBuilder {
            events: Journal::from(events),
            snapshots: self.snapshots,
            projection_offsets: self.projection_offsets,
            saga_offsets: self.saga_offsets,
        }
    }
}

impl<E, P, Sg> CustomStoreBuilder<E, Unprovided, P, Sg> {
    /// Name `snapshots` as where this deployment's aggregate snapshots are
    /// kept.
    ///
    /// Erased here as the journal is, and for the same reason.
    pub fn snapshot<S>(self, snapshots: S) -> CustomStoreBuilder<E, Album, P, Sg>
    where
        S: SnapshotStore + 'static,
    {
        CustomStoreBuilder {
            events: self.events,
            snapshots: Album::from(snapshots),
            projection_offsets: self.projection_offsets,
            saga_offsets: self.saga_offsets,
        }
    }
}

impl<E, S, Sg> CustomStoreBuilder<E, S, Unprovided, Sg> {
    /// Name `offsets` as where projections keep their applied position.
    pub fn projection_offsets<P>(self, offsets: P) -> CustomStoreBuilder<E, S, P, Sg>
    where
        P: OffsetStore + Clone + 'static,
    {
        CustomStoreBuilder {
            events: self.events,
            snapshots: self.snapshots,
            projection_offsets: offsets,
            saga_offsets: self.saga_offsets,
        }
    }
}

impl<E, S, P> CustomStoreBuilder<E, S, P, Unprovided> {
    /// Name `offsets` as where the framework's saga attachments keep their
    /// progress.
    pub fn saga_offsets<Sg>(self, offsets: Sg) -> CustomStoreBuilder<E, S, P, Sg>
    where
        Sg: OffsetStore + Clone + 'static,
    {
        CustomStoreBuilder {
            events: self.events,
            snapshots: self.snapshots,
            projection_offsets: self.projection_offsets,
            saga_offsets: offsets,
        }
    }
}

impl<E, S> CustomStoreBuilder<E, S, Unprovided, Unprovided> {
    /// Name `offsets` as where **both** kinds of offset are kept.
    ///
    /// This is the single-table deployment: one store holds a projection's
    /// position beside the framework's saga attachments', and the keys the two
    /// render are what keeps them apart. Sharing the handle is a configuration
    /// rather than a weakening of `PRV-4` — neither slot answers for what the
    /// other filed, because neither consumer renders the other's key.
    ///
    /// What it rests on is the handle's own identity: the two slots hold clones
    /// of it, and `PRV-1` is what makes those one region.
    ///
    /// Naming the two separately is
    /// [`projection_offsets`](Self::projection_offsets) and
    /// [`saga_offsets`](Self::saga_offsets), and is what has a
    /// projection rebuilt without the delivery progress beside it moving.
    pub fn offsets<C>(self, offsets: C) -> CustomStoreBuilder<E, S, C, C>
    where
        C: OffsetStore + Clone + 'static,
    {
        CustomStoreBuilder {
            events: self.events,
            snapshots: self.snapshots,
            projection_offsets: offsets.clone(),
            saga_offsets: offsets,
        }
    }
}

impl<S, P, Sg> CustomStoreBuilder<Journal, S, P, Sg>
where
    S: Clone + Send + Sync + 'static,
    P: Clone + Send + Sync + 'static,
    Sg: Clone + Send + Sync + 'static,
{
    /// The assembled store.
    pub fn build(self) -> CustomStore<S, P, Sg> {
        CustomStore {
            events: self.events,
            snapshots: self.snapshots,
            projection_offsets: self.projection_offsets,
            saga_offsets: self.saga_offsets,
        }
    }
}

impl<S, P, Sg> Store for CustomStore<S, P, Sg>
where
    S: Clone + Send + Sync + 'static,
    P: Clone + Send + Sync + 'static,
    Sg: Clone + Send + Sync + 'static,
{
    fn events(&self) -> Journal {
        self.events.clone()
    }
}

impl<P, Sg> SnapshotSlot for CustomStore<Album, P, Sg>
where
    P: Clone + Send + Sync + 'static,
    Sg: Clone + Send + Sync + 'static,
{
    fn snapshots(&self) -> Album {
        self.snapshots.clone()
    }
}

impl<S, P, Sg> ProjectionOffsetSlot for CustomStore<S, P, Sg>
where
    S: Clone + Send + Sync + 'static,
    P: OffsetStore + Clone + 'static,
    Sg: Clone + Send + Sync + 'static,
{
    type ProjectionOffsets = P;

    fn projection_offsets(&self) -> P {
        self.projection_offsets.clone()
    }
}

impl<S, P, Sg> SagaOffsetSlot for CustomStore<S, P, Sg>
where
    S: Clone + Send + Sync + 'static,
    P: Clone + Send + Sync + 'static,
    Sg: OffsetStore + Clone + 'static,
{
    type SagaOffsets = Sg;

    fn saga_offsets(&self) -> Sg {
        self.saga_offsets.clone()
    }
}
