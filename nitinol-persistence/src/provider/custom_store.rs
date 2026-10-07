//! A store assembled from handles the caller names.
//!
//! A backend may implement [`Store`] itself — the traits are public and
//! [`InMemoryStore`](crate::store::InMemoryStore) is one — but mixing backends,
//! or naming a handle a deployment already holds, is what [`CustomStore`] is
//! for: one type parameter per slot, moved from [`Unprovided`] to the handle's
//! own type by the builder call that names it.

use crate::provider::{DeliveryCheckpointSlot, ProjectionCheckpointSlot, SnapshotSlot, Store};
use crate::store::{CheckpointStore, EventStore, SnapshotStore};

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
/// Each type parameter is the handle its slot was named with, or [`Unprovided`]
/// where none was. Nothing fills a slot that was not named, which is `PRV-3`:
/// a mixture is written down or it does not happen.
///
/// Cloning this clones each handle it holds, so it reaches the regions those
/// handles name — never regions of its own.
#[derive(Clone)]
pub struct CustomStore<E, S = Unprovided, P = Unprovided, D = Unprovided> {
    events: E,
    snapshots: S,
    projection_checkpoints: P,
    delivery_checkpoints: D,
}

impl CustomStore<EventStoreUnset> {
    /// Begin assembling a store, with no handle named yet.
    pub fn builder() -> CustomStoreBuilder {
        CustomStoreBuilder {
            events: EventStoreUnset,
            snapshots: Unprovided,
            projection_checkpoints: Unprovided,
            delivery_checkpoints: Unprovided,
        }
    }
}

/// Where a [`CustomStore`]'s handles are named.
///
/// A setter exists only while its slot is unnamed, so naming one twice is a
/// compile error and the handle a slot answers with is settled in one place:
///
/// ```compile_fail
/// # use std::sync::Arc;
/// # use nitinol_persistence::provider::CustomStore;
/// # use nitinol_persistence::store::{InMemoryCheckpointStore, InMemoryEventStore};
/// // the snapshot slot has already moved, so there is no second `snapshot`
/// CustomStore::builder()
///     .eventstore(InMemoryEventStore::default())
///     .projection_checkpoints(Arc::new(InMemoryCheckpointStore::default()))
///     .projection_checkpoints(Arc::new(InMemoryCheckpointStore::default()))
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
pub struct CustomStoreBuilder<E = EventStoreUnset, S = Unprovided, P = Unprovided, D = Unprovided> {
    events: E,
    snapshots: S,
    projection_checkpoints: P,
    delivery_checkpoints: D,
}

impl<S, P, D> CustomStoreBuilder<EventStoreUnset, S, P, D> {
    /// Name `events` as the journal this deployment's streams are kept in.
    ///
    /// A handle that is not a cloneable value of its own is named through an
    /// `Arc`, which the store traits' blanket implementations make a handle in
    /// its own right.
    pub fn eventstore<E>(self, events: E) -> CustomStoreBuilder<E, S, P, D>
    where
        E: EventStore + Clone + 'static,
    {
        CustomStoreBuilder {
            events,
            snapshots: self.snapshots,
            projection_checkpoints: self.projection_checkpoints,
            delivery_checkpoints: self.delivery_checkpoints,
        }
    }
}

impl<E, P, D> CustomStoreBuilder<E, Unprovided, P, D> {
    /// Name `snapshots` as where this deployment's aggregate snapshots are
    /// kept.
    pub fn snapshot<S>(self, snapshots: S) -> CustomStoreBuilder<E, S, P, D>
    where
        S: SnapshotStore + Clone + 'static,
    {
        CustomStoreBuilder {
            events: self.events,
            snapshots,
            projection_checkpoints: self.projection_checkpoints,
            delivery_checkpoints: self.delivery_checkpoints,
        }
    }
}

impl<E, S, D> CustomStoreBuilder<E, S, Unprovided, D> {
    /// Name `checkpoints` as where projections keep their applied position.
    pub fn projection_checkpoints<P>(self, checkpoints: P) -> CustomStoreBuilder<E, S, P, D>
    where
        P: CheckpointStore + Clone + 'static,
    {
        CustomStoreBuilder {
            events: self.events,
            snapshots: self.snapshots,
            projection_checkpoints: checkpoints,
            delivery_checkpoints: self.delivery_checkpoints,
        }
    }
}

impl<E, S, P> CustomStoreBuilder<E, S, P, Unprovided> {
    /// Name `checkpoints` as where delivery hosts keep their progress.
    pub fn delivery_checkpoints<D>(self, checkpoints: D) -> CustomStoreBuilder<E, S, P, D>
    where
        D: CheckpointStore + Clone + 'static,
    {
        CustomStoreBuilder {
            events: self.events,
            snapshots: self.snapshots,
            projection_checkpoints: self.projection_checkpoints,
            delivery_checkpoints: checkpoints,
        }
    }
}

impl<E, S> CustomStoreBuilder<E, S, Unprovided, Unprovided> {
    /// Name `checkpoints` as where **both** kinds of checkpoint are kept.
    ///
    /// This is the single-table deployment: one store holds a projection's
    /// position beside a delivery host's progress, and the keys the two
    /// consumers render are what keeps them apart. Sharing the handle is a
    /// configuration rather than a weakening of `PRV-4` — neither slot answers
    /// for what the other filed, because neither consumer renders the other's
    /// key.
    ///
    /// What it rests on is the handle's own identity: the two slots hold clones
    /// of it, and `PRV-1` is what makes those one region.
    ///
    /// Naming the two separately is
    /// [`projection_checkpoints`](Self::projection_checkpoints) and
    /// [`delivery_checkpoints`](Self::delivery_checkpoints), and is what has a
    /// projection rebuilt without the delivery progress beside it moving.
    pub fn checkpoints<C>(self, checkpoints: C) -> CustomStoreBuilder<E, S, C, C>
    where
        C: CheckpointStore + Clone + 'static,
    {
        CustomStoreBuilder {
            events: self.events,
            snapshots: self.snapshots,
            projection_checkpoints: checkpoints.clone(),
            delivery_checkpoints: checkpoints,
        }
    }
}

impl<E, S, P, D> CustomStoreBuilder<E, S, P, D>
where
    E: EventStore + Clone + 'static,
    S: Clone + Send + Sync + 'static,
    P: Clone + Send + Sync + 'static,
    D: Clone + Send + Sync + 'static,
{
    /// The assembled store.
    pub fn build(self) -> CustomStore<E, S, P, D> {
        CustomStore {
            events: self.events,
            snapshots: self.snapshots,
            projection_checkpoints: self.projection_checkpoints,
            delivery_checkpoints: self.delivery_checkpoints,
        }
    }
}

impl<E, S, P, D> Store for CustomStore<E, S, P, D>
where
    E: EventStore + Clone + 'static,
    S: Clone + Send + Sync + 'static,
    P: Clone + Send + Sync + 'static,
    D: Clone + Send + Sync + 'static,
{
    type Events = E;

    fn events(&self) -> E {
        self.events.clone()
    }
}

impl<E, S, P, D> SnapshotSlot for CustomStore<E, S, P, D>
where
    E: EventStore + Clone + 'static,
    S: SnapshotStore + Clone + 'static,
    P: Clone + Send + Sync + 'static,
    D: Clone + Send + Sync + 'static,
{
    type Snapshots = S;

    fn snapshots(&self) -> S {
        self.snapshots.clone()
    }
}

impl<E, S, P, D> ProjectionCheckpointSlot for CustomStore<E, S, P, D>
where
    E: EventStore + Clone + 'static,
    S: Clone + Send + Sync + 'static,
    P: CheckpointStore + Clone + 'static,
    D: Clone + Send + Sync + 'static,
{
    type ProjectionCheckpoints = P;

    fn projection_checkpoints(&self) -> P {
        self.projection_checkpoints.clone()
    }
}

impl<E, S, P, D> DeliveryCheckpointSlot for CustomStore<E, S, P, D>
where
    E: EventStore + Clone + 'static,
    S: Clone + Send + Sync + 'static,
    P: Clone + Send + Sync + 'static,
    D: CheckpointStore + Clone + 'static,
{
    type DeliveryCheckpoints = D;

    fn delivery_checkpoints(&self) -> D {
        self.delivery_checkpoints.clone()
    }
}
