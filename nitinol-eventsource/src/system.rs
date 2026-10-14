use std::sync::Arc;

use crate::projection::ProjectionId;
use nitinol_contract::Aggregate;
use nitinol_persistence::provider::{
    PersistenceProvider, ProjectionOffsetSlot, SnapshotSlot, Store,
};
use nitinol_persistence::store::Journal;
use nitinol_persistence::AggregateId;
use nitinol_runtime::ProcessSystem;
use tokio::sync::OnceCell;

use crate::process::resolve::{AggregateRegistry, ResolveHandle};
use crate::process::{SnapshotPersistor, SnapshotPersistorProxy};
use crate::projection::ProjectorProps;
use crate::{AggregateProps, AggregateProxy};

/// Marker type indicating no persistence supplier has been supplied yet.
///
/// A builder in this state has no [`build`][EventSourceSystemBuilder::build],
/// so there is no way to reach a system whose stores nobody named.
pub struct PersistenceUnset;

/// The system a handle onto any store erases to.
///
/// Wiring whose types are fixed before a deployment's store is known — a saga's
/// dispatch mapping names the system type it delivers through when the mapping
/// is written — names this one type for every store. It reaches the same
/// process system, the same aggregate registry and the same journal the system
/// it was erased from does.
pub type SystemHandle = EventSourceSystem<Journal>;

/// Props that carry no system, so the caller names the process system at the
/// spawn.
///
/// This is the low-level form: a lifecycle pinned to one aggregate, or a
/// component started where there is no system to take one from.
pub struct Unhosted;

/// Props that carry the system they came from, so the spawn needs nothing
/// re-injected.
///
/// Every entry point on [`EventSourceSystem`] produces props in this state.
pub struct Hosted(SystemHandle);

impl Hosted {
    /// Carry `host` as where the props holding this start.
    ///
    /// Public because the saga entry points that produce hosted props live in
    /// `nitinol-saga` — the dependency arrow points `saga -> eventsource`, so
    /// `host` and `deliver` are an extension trait there rather than inherent
    /// methods here, and they need the same state this crate's own entry
    /// points put their props in.
    pub fn new(host: SystemHandle) -> Self {
        Self(host)
    }

    /// The system these props were taken from.
    pub fn system(&self) -> &SystemHandle {
        &self.0
    }
}

/// Builder for [`EventSourceSystem`].
///
/// Obtained from [`EventSourceSystem::builder`]. Supply the deployment's store
/// with [`with_persistence`][Self::with_persistence], then
/// [`build`][EventSourceSystemBuilder::build].
///
/// There is no codec stage. Every event type names the codec that writes it, so
/// a system has no format of its own to bind and no way to disagree with the
/// events it spawns aggregates for.
///
/// # Builder
/// ```rust,no_run
/// # use nitinol_eventsource::system::EventSourceSystem;
/// # use nitinol_persistence::provider::PersistenceProvider;
/// # use nitinol_persistence::store::InMemoryStore;
/// # use nitinol_runtime::ProcessSystem;
/// # async fn wiring() {
/// # let ps = ProcessSystem::new().await;
/// let system = EventSourceSystem::builder(ps)
///     .with_persistence(PersistenceProvider::new(InMemoryStore::default()))
///     .build();
/// # }
/// ```
pub struct EventSourceSystemBuilder<P = PersistenceUnset> {
    ps: ProcessSystem,
    persistence: P,
}

impl EventSourceSystemBuilder<PersistenceUnset> {
    fn new(ps: ProcessSystem) -> Self {
        Self {
            ps,
            persistence: PersistenceUnset,
        }
    }

    /// Supply the store this system's wiring resolves from.
    ///
    /// # One store, every stream
    ///
    /// [`EventStore`](nitinol_persistence::store::EventStore) is stream-keyed —
    /// `append(key, ..)` and
    /// [`LoadQuery::by_stream`](nitinol_persistence::LoadQuery::by_stream)
    /// address one stream at a time — so the single journal a store supplies
    /// holds every aggregate's stream (and every saga's journal) side by side,
    /// each owner still reading only its own key. That is what makes *one*
    /// supplier sufficient rather than a registry of named stores.
    ///
    /// Placing one aggregate's stream in a different destination is therefore a
    /// second supplier, assembled and named as such, with
    /// [`AggregateProps::new`] built on the store it distributes. There is no
    /// per-spawn override: the entry points below all resolve the one supplier
    /// this system carries, so a deployment cannot half-move a stream and leave
    /// two activations writing to two worlds.
    ///
    /// # What a supplier does not promise
    ///
    /// Nothing atomic across destinations. One supplier is not one transaction
    /// — the clauses on [`Store`] state what it does and does not guarantee.
    pub fn with_persistence<St: Store>(
        self,
        persistence: PersistenceProvider<St>,
    ) -> EventSourceSystemBuilder<PersistenceProvider<St>> {
        EventSourceSystemBuilder {
            ps: self.ps,
            persistence,
        }
    }
}

impl<St: Store> EventSourceSystemBuilder<PersistenceProvider<St>> {
    /// Finalize construction and return the configured [`EventSourceSystem`].
    pub fn build(self) -> EventSourceSystem<St> {
        EventSourceSystem {
            ps: self.ps,
            registry: AggregateRegistry::new(),
            persistence: Arc::new(self.persistence),
            snapshot_persistor: Arc::new(OnceCell::new()),
        }
    }
}

/// System-level orchestrator that binds a [`ProcessSystem`] to the store a
/// deployment's records live in.
///
/// `St` is that store, and which wiring entry points exist is decided by the
/// slots it holds — so a store nobody configured cannot be relied on:
///
/// | The store holds | Entry points |
/// |---|---|
/// | the journal ([`Store`]) | [`spawn_aggregate`][Self::spawn_aggregate] / [`aggregate_props`][Self::aggregate_props] / [`aggregate_proxy`][Self::aggregate_proxy] |
/// | a snapshot store ([`SnapshotSlot`]) | [`snapshot_persistor`][Self::snapshot_persistor] |
/// | projection offsets ([`ProjectionOffsetSlot`]) | [`projector_props`][Self::projector_props] |
///
/// A slot the store does not hold is a **compile error** at the wiring that
/// needs it, rather than a failure at the first save — which is the whole of
/// why the store is a type parameter here.
///
/// The wire format is not among the things a system settles: an aggregate's
/// events name the codec that writes them, so two aggregates spawned from one
/// system read and write their own streams in their own formats.
///
/// # Resolving, not spawning
///
/// Every aggregate entry point below **resolves**: an identifier is mapped to a
/// reference, and the aggregate is activated only if this node has no activation
/// for it yet.  Concurrent resolves of one identifier cause at most one
/// activation and all receive the same reference, and nothing observable says
/// which call did the activating.
///
/// Two things follow that code written against this system must respect.
///
/// * **Duplicate activations are possible and normal.**  The at-most-one
///   guarantee is per node.  Across a cluster, one aggregate may be activated
///   more than once at the same time — that is a placement goal and a
///   convergence property, not a safety contract.  Safety belongs to the event
///   store: an append derived from a state the stream has moved past is
///   rejected, and the activation that made it stops.  Never write code whose
///   correctness needs single activation.
/// * **Store-external side effects are not at-most-once.**  A bare
///   [`tell`](crate::AggregateProxy::tell) leaves the store's arbitration, so a
///   duplicate activation can perform it a second time and nothing detects it.
///   A side effect that must be cluster-safe is expressed as a persisted record
///   — the events of a [`Decision`](crate::Decision), or a saga's own decision
///   carried by a delivery host — so that some stream's own OCC decides whether
///   it happened at all.
///
/// Starting a lifecycle explicitly, rather than resolving one, is
/// [`AggregateProps::spawn`] on props built directly.
///
/// # A handle, not an instance
///
/// Cloning an `EventSourceSystem` names the same instance rather than creating a
/// second one: every clone resolves through the same aggregate registry, runs on
/// the same [`ProcessSystem`], carries the same supplier, and shares the one
/// snapshot persistor process.  The per-node at-most-one guarantee therefore
/// holds across clones — two clones resolving one identity reach one activation
/// — and callers share the system by cloning it rather than by wrapping it in an
/// `Arc`.
///
/// The entry points stay by-reference so that whether to clone remains the
/// caller's decision.  Configuration is fixed before any handle exists: the
/// supplier is supplied on [`EventSourceSystemBuilder`], and
/// [`build`][EventSourceSystemBuilder::build] settles it.
///
/// # What holds what
///
/// A system holds its process system, its registry, its supplier and the cell
/// the snapshot persistor process lands in — and nothing on the other side of
/// those holds a system back. The registry keeps process proxies, a process
/// proxy keeps a pid and a channel, and a persistor keeps a snapshot store, so
/// there is no strong cycle to leak: a handle lives as long as the processes
/// holding it, and is released when they stop.
pub struct EventSourceSystem<St = PersistenceUnset> {
    /// A handle rather than an owned instance: a reference re-resolves its
    /// aggregate long after the entry point that produced it returned, and needs
    /// a system to activate on.
    ps: ProcessSystem,
    registry: Arc<AggregateRegistry>,
    persistence: Arc<PersistenceProvider<St>>,
    /// The one snapshot persistor process this system distributes.
    ///
    /// Shared across clones and across the erased handle, because the snapshot
    /// contract is written for a single consumer serialising its calls: a second
    /// process over one store would be two.
    snapshot_persistor: Arc<OnceCell<SnapshotPersistorProxy>>,
}

/// Written out rather than derived: a clone names the same instance whether or
/// not the store type itself is `Clone`, and requiring `St: Clone` would make
/// an assembly's choice of handle types decide whether its system can be
/// shared.
impl<St> Clone for EventSourceSystem<St> {
    fn clone(&self) -> Self {
        Self {
            ps: self.ps.clone(),
            registry: Arc::clone(&self.registry),
            persistence: Arc::clone(&self.persistence),
            snapshot_persistor: Arc::clone(&self.snapshot_persistor),
        }
    }
}

impl EventSourceSystem<PersistenceUnset> {
    /// Begin building an [`EventSourceSystem`] backed by the given [`ProcessSystem`].
    pub fn builder(ps: ProcessSystem) -> EventSourceSystemBuilder<PersistenceUnset> {
        EventSourceSystemBuilder::new(ps)
    }
}

impl<St> EventSourceSystem<St> {
    /// Returns a reference to the underlying [`ProcessSystem`].
    pub fn process_system(&self) -> &ProcessSystem {
        &self.ps
    }

    /// The supplier this system was built with.
    ///
    /// Wiring that takes a store as a value — an upstream subscription, a saga's
    /// own journal — resolves it from here rather than keeping a second `Arc`
    /// alongside the system.
    pub fn persistence(&self) -> &PersistenceProvider<St> {
        &self.persistence
    }

    /// What a reference needs to resolve, and re-resolve, its aggregate here.
    pub(crate) fn resolve_handle(&self) -> ResolveHandle {
        ResolveHandle::new(Arc::clone(&self.registry), self.ps.clone())
    }
}

impl<St: Store> EventSourceSystem<St> {
    /// Resolve the aggregate `id`, activating it if this node has no activation
    /// for it, and return a reference to it.
    ///
    /// The aggregate's stream lives under `id` in the journal the store holds;
    /// other aggregates, and any saga journalling through this system, are
    /// tenants of the same instance under their own keys.
    ///
    /// # Typestate
    ///
    /// There is no form of this that names a store, and no system without a
    /// supplier to resolve one from — a builder that was never handed one has
    /// no `build`, so the call below is a **compile error**:
    ///
    /// ```compile_fail
    /// # use nitinol_eventsource::system::EventSourceSystem;
    /// # use nitinol_runtime::ProcessSystem;
    /// # async fn wiring() {
    /// # let ps = ProcessSystem::new().await;
    /// // no `with_persistence` — there are no stores to spawn onto
    /// let system = EventSourceSystem::builder(ps).build();
    /// # }
    /// ```
    ///
    /// Nor is there a per-spawn override to reach a second destination with.
    /// The entry point that took one is gone:
    ///
    /// ```compile_fail
    /// # use nitinol_eventsource::system::EventSourceSystem;
    /// # use nitinol_persistence::store::InMemoryEventStore;
    /// # use nitinol_runtime::ProcessSystem;
    /// # async fn wiring() {
    /// # let ps = ProcessSystem::new().await;
    /// # let store = InMemoryEventStore::default();
    /// // `with_event_store` does not exist: the stores come from a supplier
    /// let system = EventSourceSystem::builder(ps).with_event_store(store).build();
    /// # }
    /// ```
    pub async fn spawn_aggregate<A: Aggregate>(&self, id: AggregateId) -> AggregateProxy<A> {
        self.aggregate_props::<A>(id).spawn().await
    }

    /// Return an [`AggregateProps`] on the journal the store holds, carrying
    /// this system as where it starts.
    ///
    /// Use this when additional configuration (e.g., a snapshot persistor) is
    /// needed before spawning.  Its `spawn` takes no argument and resolves
    /// exactly as [`spawn_aggregate`][Self::spawn_aggregate] does.
    pub fn aggregate_props<A: Aggregate>(&self, id: AggregateId) -> AggregateProps<A, Hosted> {
        AggregateProps::<A>::new(id, self.persistence.event_store()).hosted(self.erase_store())
    }

    /// A reference to the aggregate `id`, built without awaiting and without
    /// activating anything.
    ///
    /// The aggregate is resolved on the reference's first dispatch, and resolved
    /// again after the activation it reached dies — the same reference the
    /// awaited entry points hand back, obtained where awaiting is not possible: a
    /// `fold` over a decision, or a saga's delivery addressing a target it has
    /// not yet activated.
    ///
    /// Deferring the activation is what makes that sound.  A reference names the
    /// aggregate, not a process, so building one commits to nothing that could
    /// have become stale by the time it is used.
    pub fn aggregate_proxy<A: Aggregate>(&self, id: AggregateId) -> AggregateProxy<A> {
        self.aggregate_props::<A>(id).into_reference()
    }

    /// The same system, with the store's type erased.
    ///
    /// It is a handle, not a copy: the same process system, the same aggregate
    /// registry, the same journal and the same snapshot persistor. Resolving an
    /// identity through the erased handle reaches the activation the original
    /// would.
    ///
    /// Only the journal survives the erasure — a [`Journal`] *is* the store
    /// holding the journal alone, and it holds no other slot — so a slot a
    /// deployment did not supply stays a compile error rather than becoming
    /// reachable through a handle that lost the type saying so:
    ///
    /// ```compile_fail
    /// # use nitinol_eventsource::system::EventSourceSystem;
    /// # use nitinol_persistence::provider::PersistenceProvider;
    /// # use nitinol_persistence::store::InMemoryStore;
    /// # use nitinol_runtime::ProcessSystem;
    /// # async fn wiring() {
    /// # let ps = ProcessSystem::new().await;
    /// let system = EventSourceSystem::builder(ps)
    ///     .with_persistence(PersistenceProvider::new(InMemoryStore::default()))
    ///     .build();
    /// // the erased handle has no snapshot slot, whatever the store it came
    /// // from supplied
    /// system.erase_store().snapshot_persistor().await;
    /// # }
    /// ```
    pub fn erase_store(&self) -> SystemHandle {
        EventSourceSystem {
            ps: self.ps.clone(),
            registry: Arc::clone(&self.registry),
            persistence: Arc::new(PersistenceProvider::new(self.persistence.event_store())),
            snapshot_persistor: Arc::clone(&self.snapshot_persistor),
        }
    }
}

impl<St: SnapshotSlot> EventSourceSystem<St> {
    /// The handle onto this system's snapshot persistor, starting it on the
    /// first call.
    ///
    /// One process per system, shared by every caller and every clone: the
    /// snapshot contract is written for a single consumer serialising its calls,
    /// and two processes over one store would be two consumers.
    ///
    /// # Typestate
    ///
    /// This exists only where the store holds a snapshot store. One that holds
    /// none is refused here rather than at the first save:
    ///
    /// ```compile_fail
    /// # use nitinol_eventsource::system::EventSourceSystem;
    /// # use nitinol_persistence::provider::{CustomStore, PersistenceProvider};
    /// # use nitinol_persistence::store::InMemoryEventStore;
    /// # use nitinol_runtime::ProcessSystem;
    /// # async fn wiring() {
    /// # let ps = ProcessSystem::new().await;
    /// // no `snapshot` — this store has no snapshot slot
    /// let store = CustomStore::builder().eventstore(InMemoryEventStore::default()).build();
    /// let system = EventSourceSystem::builder(ps)
    ///     .with_persistence(PersistenceProvider::new(store))
    ///     .build();
    /// system.snapshot_persistor().await;
    /// # }
    /// ```
    pub async fn snapshot_persistor(&self) -> SnapshotPersistorProxy {
        self.snapshot_persistor
            .get_or_init(|| async { SnapshotPersistor::spawn(&self.ps, &self.persistence).await })
            .await
            .clone()
    }
}

impl<St: ProjectionOffsetSlot> EventSourceSystem<St> {
    /// Return a [`ProjectorProps`] over the journal and the projection
    /// offsets the store holds, carrying this system as where it starts.
    ///
    /// Both slots are resolved here, so a projector built from a system follows
    /// the same journal its aggregates write to and records its position where
    /// the deployment's other projections record theirs.
    ///
    /// # Typestate
    ///
    /// This exists only where the store holds projection offsets:
    ///
    /// ```compile_fail
    /// # use nitinol_eventsource::system::EventSourceSystem;
    /// # use nitinol_persistence::provider::{CustomStore, PersistenceProvider};
    /// # use nitinol_persistence::store::InMemoryEventStore;
    /// # use nitinol_eventsource::ProjectionId;
    /// # use nitinol_runtime::ProcessSystem;
    /// # struct ReadModel;
    /// # async fn wiring() {
    /// # let ps = ProcessSystem::new().await;
    /// // no `projection_offsets` — this store has no projection slot
    /// let store = CustomStore::builder().eventstore(InMemoryEventStore::default()).build();
    /// let system = EventSourceSystem::builder(ps)
    ///     .with_persistence(PersistenceProvider::new(store))
    ///     .build();
    /// system.projector_props(ProjectionId::new("read-model"), || ReadModel);
    /// # }
    /// ```
    pub fn projector_props<Proj>(
        &self,
        projection_id: ProjectionId,
        producer: impl Fn() -> Proj + Send + Sync + 'static,
    ) -> ProjectorProps<
        Proj,
        St::ProjectionOffsets,
        (),
        crate::projection::EventUnset,
        crate::projection::OriginUnset,
        Hosted,
    >
    where
        Proj: Send + Sync + 'static,
    {
        ProjectorProps::new(projection_id, &self.persistence, producer).hosted(self.erase_store())
    }
}
