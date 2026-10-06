//! The assembly a deployment names its handles in.
//!
//! A provider can be written from scratch — the traits are public and a backend
//! is free to implement them — but what most deployments need is to name a
//! handle per slot and be told at compile time which wiring that supports.
//! [`Persistence`] is that: one type parameter per optional slot, moved from
//! [`Unprovided`] to a provided state by the `with_*` call that names the
//! store.

use std::sync::Arc;

use crate::provider::{
    DeliveryCheckpointProvider, PersistenceProvider, ProjectionCheckpointProvider,
    SnapshotStoreProvider,
};
use crate::store::{
    CheckpointStore, EventStore, InMemoryCheckpointStore, InMemoryEventStore,
    InMemorySnapshotStore, SnapshotStore,
};

/// A slot no store has been named for.
///
/// The wiring that needs it does not exist on a provider in this state, so
/// "nobody configured a snapshot store" is a compile error at the wiring rather
/// than a failure at the first save.
#[derive(Clone, Copy)]
pub struct Unprovided;

/// The snapshot slot, carrying the store it was named with.
pub struct SnapshotStoreProvided {
    store: Arc<dyn SnapshotStore>,
}

/// The projection-checkpoint slot, carrying the store it was named with.
pub struct ProjectionCheckpointsProvided<Cs> {
    store: Arc<Cs>,
}

/// The delivery-checkpoint slot, carrying the store it was named with.
pub struct DeliveryCheckpointsProvided<Cs> {
    store: Arc<Cs>,
}

/// Cloning a slot names the same store: the handle is what is copied, never the
/// data region behind it. Written out rather than derived because the store
/// itself is not `Clone` and does not need to be.
impl Clone for SnapshotStoreProvided {
    fn clone(&self) -> Self {
        Self {
            store: Arc::clone(&self.store),
        }
    }
}

impl<Cs> Clone for ProjectionCheckpointsProvided<Cs> {
    fn clone(&self) -> Self {
        Self {
            store: Arc::clone(&self.store),
        }
    }
}

impl<Cs> Clone for DeliveryCheckpointsProvided<Cs> {
    fn clone(&self) -> Self {
        Self {
            store: Arc::clone(&self.store),
        }
    }
}

/// A [`PersistenceProvider`] assembled from named handles.
///
/// The journal is required, so [`new`](Persistence::new) takes it. Each further
/// slot is named by the `with_*` call for it, which exists only while that slot
/// is [`Unprovided`] — naming one twice is a compile error, so the store a slot
/// answers with is settled in one place.
///
/// Mixing backends is exactly this: naming a handle per slot, from wherever
/// each comes from. Nothing fills a slot that was not named.
///
/// # A handle, not a region
///
/// Cloning a `Persistence` names the same stores. The slots answer with clones
/// of the handles they hold, so every consumer wired from one clone and every
/// consumer wired from another reach one set of records — which is `PRV-1`
/// across clones as well as across calls.
#[derive(Clone)]
pub struct Persistence<S = Unprovided, P = Unprovided, D = Unprovided> {
    event_store: Arc<dyn EventStore>,
    snapshots: S,
    projection_checkpoints: P,
    delivery_checkpoints: D,
}

impl Persistence<Unprovided, Unprovided, Unprovided> {
    /// A provider over `event_store`, with no other slot named yet.
    pub fn new(event_store: Arc<dyn EventStore>) -> Self {
        Self {
            event_store,
            snapshots: Unprovided,
            projection_checkpoints: Unprovided,
            delivery_checkpoints: Unprovided,
        }
    }
}

impl<P, D> Persistence<Unprovided, P, D> {
    /// Name `store` as where this deployment's aggregate snapshots are kept.
    pub fn with_snapshot_store(
        self,
        store: Arc<dyn SnapshotStore>,
    ) -> Persistence<SnapshotStoreProvided, P, D> {
        Persistence {
            event_store: self.event_store,
            snapshots: SnapshotStoreProvided { store },
            projection_checkpoints: self.projection_checkpoints,
            delivery_checkpoints: self.delivery_checkpoints,
        }
    }
}

impl<S, D> Persistence<S, Unprovided, D> {
    /// Name `store` as where projections keep their applied position.
    pub fn with_projection_checkpoints<Cs>(
        self,
        store: Arc<Cs>,
    ) -> Persistence<S, ProjectionCheckpointsProvided<Cs>, D>
    where
        Cs: CheckpointStore + 'static,
    {
        Persistence {
            event_store: self.event_store,
            snapshots: self.snapshots,
            projection_checkpoints: ProjectionCheckpointsProvided { store },
            delivery_checkpoints: self.delivery_checkpoints,
        }
    }
}

impl<S, P> Persistence<S, P, Unprovided> {
    /// Name `store` as where delivery hosts keep their progress.
    pub fn with_delivery_checkpoints<Cs>(
        self,
        store: Arc<Cs>,
    ) -> Persistence<S, P, DeliveryCheckpointsProvided<Cs>>
    where
        Cs: CheckpointStore + 'static,
    {
        Persistence {
            event_store: self.event_store,
            snapshots: self.snapshots,
            projection_checkpoints: self.projection_checkpoints,
            delivery_checkpoints: DeliveryCheckpointsProvided { store },
        }
    }
}

impl<S> Persistence<S, Unprovided, Unprovided> {
    /// Name `store` as where **both** kinds of checkpoint are kept.
    ///
    /// This is the single-table deployment: one store holds a projection's
    /// position beside a delivery host's progress, and the keys the two
    /// consumers render are what keeps them apart. Sharing the store is a
    /// configuration rather than a weakening of `PRV-4` — neither slot answers
    /// for what the other filed, because neither consumer renders the other's
    /// key.
    ///
    /// Naming the two separately is [`with_projection_checkpoints`] and
    /// [`with_delivery_checkpoints`], and is what has a projection rebuilt
    /// without the delivery progress beside it moving.
    ///
    /// [`with_projection_checkpoints`]: Persistence::with_projection_checkpoints
    /// [`with_delivery_checkpoints`]: Persistence::with_delivery_checkpoints
    pub fn with_checkpoints<Cs>(
        self,
        store: Arc<Cs>,
    ) -> Persistence<S, ProjectionCheckpointsProvided<Cs>, DeliveryCheckpointsProvided<Cs>>
    where
        Cs: CheckpointStore + 'static,
    {
        Persistence {
            event_store: self.event_store,
            snapshots: self.snapshots,
            projection_checkpoints: ProjectionCheckpointsProvided {
                store: Arc::clone(&store),
            },
            delivery_checkpoints: DeliveryCheckpointsProvided { store },
        }
    }
}

impl<S, P, D> PersistenceProvider for Persistence<S, P, D>
where
    S: Send + Sync + 'static,
    P: Send + Sync + 'static,
    D: Send + Sync + 'static,
{
    fn event_store(&self) -> Arc<dyn EventStore> {
        Arc::clone(&self.event_store)
    }
}

impl<P, D> SnapshotStoreProvider for Persistence<SnapshotStoreProvided, P, D>
where
    P: Send + Sync + 'static,
    D: Send + Sync + 'static,
{
    fn snapshot_store(&self) -> Arc<dyn SnapshotStore> {
        Arc::clone(&self.snapshots.store)
    }
}

impl<S, Cs, D> ProjectionCheckpointProvider for Persistence<S, ProjectionCheckpointsProvided<Cs>, D>
where
    S: Send + Sync + 'static,
    Cs: CheckpointStore + 'static,
    D: Send + Sync + 'static,
{
    type ProjectionCheckpoints = Cs;

    fn projection_checkpoints(&self) -> Arc<Cs> {
        Arc::clone(&self.projection_checkpoints.store)
    }
}

impl<S, P, Cs> DeliveryCheckpointProvider for Persistence<S, P, DeliveryCheckpointsProvided<Cs>>
where
    S: Send + Sync + 'static,
    P: Send + Sync + 'static,
    Cs: CheckpointStore + 'static,
{
    type DeliveryCheckpoints = Cs;

    fn delivery_checkpoints(&self) -> Arc<Cs> {
        Arc::clone(&self.delivery_checkpoints.store)
    }
}

/// A [`Persistence`] with every slot named, over the in-memory reference
/// stores.
///
/// Both checkpoint slots are the same store, which is the single-table shape a
/// real backend is deployed in.
pub type InMemoryPersistence = Persistence<
    SnapshotStoreProvided,
    ProjectionCheckpointsProvided<InMemoryCheckpointStore>,
    DeliveryCheckpointsProvided<InMemoryCheckpointStore>,
>;

impl InMemoryPersistence {
    /// A provider over fresh in-memory stores, holding nothing.
    ///
    /// Intended for tests and examples; the stores it names are not for
    /// production use.
    pub fn in_memory() -> Self {
        let checkpoints = Arc::new(InMemoryCheckpointStore::default());
        Persistence::new(Arc::new(InMemoryEventStore::default()))
            .with_snapshot_store(Arc::new(InMemorySnapshotStore::default()))
            .with_checkpoints(checkpoints)
    }
}
