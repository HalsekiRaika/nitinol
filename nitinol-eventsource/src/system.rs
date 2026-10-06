use std::sync::Arc;

use nitinol_contract::Aggregate;
use nitinol_persistence::provider::{
    PersistenceProvider, ProjectionCheckpointProvider, SnapshotStoreProvider,
};
use nitinol_persistence::store::EventStore;
use nitinol_persistence::{AggregateId, ProjectionId};
use nitinol_runtime::ProcessSystem;
use tokio::sync::OnceCell;

use crate::process::resolve::{AggregateRegistry, ResolveHandle};
use crate::process::{SnapshotPersistor, SnapshotPersistorProxy};
use crate::projection::ProjectorProps;
use crate::{AggregateProps, AggregateProxy};

/// Marker type indicating no persistence provider has been supplied yet.
///
/// A builder in this state has no [`build`][EventSourceSystemBuilder::build],
/// so there is no way to reach a system whose stores nobody named.
pub struct PersistenceUnset;

/// Builder for [`EventSourceSystem`].
///
/// Obtained from [`EventSourceSystem::builder`]. Supply the deployment's stores
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
/// # use nitinol_persistence::provider::Persistence;
/// # use nitinol_runtime::ProcessSystem;
/// # async fn wiring() {
/// # let ps = ProcessSystem::new().await;
/// let system = EventSourceSystem::builder(ps)
///     .with_persistence(Persistence::in_memory())
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

    /// Supply the stores this system's wiring resolves from.
    ///
    /// # One store, every stream
    ///
    /// [`EventStore`] is stream-keyed — `append(key, ..)` and
    /// [`LoadQuery::by_stream`](nitinol_persistence::LoadQuery::by_stream)
    /// address one stream at a time — so the single journal a provider supplies
    /// holds every aggregate's stream (and every saga's journal) side by side,
    /// each owner still reading only its own key. That is what makes *one*
    /// supplier sufficient rather than a registry of named stores.
    ///
    /// Placing one aggregate's stream in a different destination is therefore a
    /// second provider, assembled and named as such, with
    /// [`AggregateProps::new`] built on the store it supplies. There is no
    /// per-spawn override: the entry points below all resolve the one provider
    /// this system carries, so a deployment cannot half-move a stream and leave
    /// two activations writing to two worlds.
    ///
    /// # What a provider does not promise
    ///
    /// Nothing atomic across destinations. One supplier is not one transaction
    /// — the clauses on
    /// [`PersistenceProvider`](nitinol_persistence::provider::PersistenceProvider)
    /// state what it does and does not guarantee.
    pub fn with_persistence<P: PersistenceProvider>(
        self,
        persistence: P,
    ) -> EventSourceSystemBuilder<P> {
        EventSourceSystemBuilder {
            ps: self.ps,
            persistence,
        }
    }
}

impl<P: PersistenceProvider> EventSourceSystemBuilder<P> {
    /// Finalize construction and return the configured [`EventSourceSystem`].
    pub fn build(self) -> EventSourceSystem<P> {
        EventSourceSystem {
            ps: self.ps,
            registry: AggregateRegistry::new(),
            persistence: Arc::new(self.persistence),
            snapshot_persistor: Arc::new(OnceCell::new()),
        }
    }
}

/// System-level orchestrator that binds a [`ProcessSystem`] to the persistence
/// provider a deployment's stores come from.
///
/// `P` is that provider, and which wiring entry points exist is decided by the
/// slots it supplies — so a store nobody configured cannot be relied on:
///
/// | The provider supplies | Entry points |
/// |---|---|
/// | the journal ([`PersistenceProvider`]) | [`spawn_aggregate`][Self::spawn_aggregate] / [`aggregate_props`][Self::aggregate_props] / [`aggregate_proxy`][Self::aggregate_proxy] |
/// | a snapshot store ([`SnapshotStoreProvider`]) | [`snapshot_persistor`][Self::snapshot_persistor] |
/// | projection checkpoints ([`ProjectionCheckpointProvider`]) | [`projector_props`][Self::projector_props] |
///
/// A slot the provider does not supply is a **compile error** at the wiring
/// that needs it, rather than a failure at the first save — which is the whole
/// of why the provider is a type parameter here.
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
///   — the events of a [`Decision`](crate::Decision), or a saga outbox — so that
///   some stream's own OCC decides whether it happened at all.
///
/// Starting a lifecycle explicitly, rather than resolving one, is
/// [`AggregateProps::spawn`] on props built directly.
///
/// # A handle, not an instance
///
/// Cloning an `EventSourceSystem` names the same instance rather than creating a
/// second one: every clone resolves through the same aggregate registry, runs on
/// the same [`ProcessSystem`], carries the same provider, and shares the one
/// snapshot persistor process.  The per-node at-most-one guarantee therefore
/// holds across clones — two clones resolving one identity reach one activation
/// — and callers share the system by cloning it rather than by wrapping it in an
/// `Arc`.
///
/// The entry points stay by-reference so that whether to clone remains the
/// caller's decision.  Configuration is fixed before any handle exists: the
/// provider is supplied on [`EventSourceSystemBuilder`], and
/// [`build`][EventSourceSystemBuilder::build] settles it.
pub struct EventSourceSystem<P = PersistenceUnset> {
    /// A handle rather than an owned instance: a reference re-resolves its
    /// aggregate long after the entry point that produced it returned, and needs
    /// a system to activate on.
    ps: ProcessSystem,
    registry: Arc<AggregateRegistry>,
    persistence: Arc<P>,
    /// The one snapshot persistor process this system distributes.
    ///
    /// Shared across clones and across the erased handle, because the snapshot
    /// contract is written for a single consumer serialising its calls: a second
    /// process over one store would be two.
    snapshot_persistor: Arc<OnceCell<SnapshotPersistorProxy>>,
}

/// Written out rather than derived: a clone names the same instance whether or
/// not the provider type itself is `Clone`, and requiring `P: Clone` would
/// make an assembly's choice of handle types decide whether its system can be
/// shared.
impl<P> Clone for EventSourceSystem<P> {
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

impl<P> EventSourceSystem<P> {
    /// Returns a reference to the underlying [`ProcessSystem`].
    pub fn process_system(&self) -> &ProcessSystem {
        &self.ps
    }

    /// The persistence provider this system was built with.
    ///
    /// Wiring that takes a store as a value — an upstream subscription, a saga's
    /// own journal — resolves it from here rather than keeping a second `Arc`
    /// alongside the system.
    pub fn persistence(&self) -> &P {
        &self.persistence
    }

    /// What a reference needs to resolve, and re-resolve, its aggregate here.
    fn resolve_handle(&self) -> ResolveHandle {
        ResolveHandle::new(Arc::clone(&self.registry), self.ps.clone())
    }

    /// [`AggregateProps`] for `id` on `store`.
    ///
    /// The one place a store is joined into props, so every entry point resolves
    /// to the same wiring.
    fn props_on<A: Aggregate>(
        &self,
        id: AggregateId,
        store: Arc<dyn EventStore>,
    ) -> AggregateProps<A> {
        AggregateProps::<A>::new(id, store)
    }

    /// The same props, wired to resolve through this system's registry rather
    /// than to activate per call.
    fn resolving_props_on<A: Aggregate>(
        &self,
        id: AggregateId,
        store: Arc<dyn EventStore>,
    ) -> AggregateProps<A> {
        self.props_on::<A>(id, store)
            .with_resolve(self.resolve_handle())
    }

    /// A reference to the aggregate `id` on `store`, resolved at dispatch time.
    fn reference_on<A: Aggregate>(
        &self,
        id: AggregateId,
        store: Arc<dyn EventStore>,
    ) -> AggregateProxy<A> {
        self.props_on::<A>(id, store)
            .into_reference(self.resolve_handle())
    }
}

impl<P: PersistenceProvider> EventSourceSystem<P> {
    /// Resolve the aggregate `id`, activating it if this node has no activation
    /// for it, and return a reference to it.
    ///
    /// The aggregate's stream lives under `id` in the journal the provider
    /// supplies; other aggregates, and any saga journalling through this system,
    /// are tenants of the same instance under their own keys.
    ///
    /// # Typestate
    ///
    /// There is no form of this that names a store, and no system without a
    /// provider to resolve one from — a builder that was never handed a provider
    /// has no `build`, so the call below is a **compile error**:
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
    /// # use std::sync::Arc;
    /// # use nitinol_eventsource::system::EventSourceSystem;
    /// # use nitinol_persistence::provider::Persistence;
    /// # use nitinol_persistence::store::{EventStore, InMemoryEventStore};
    /// # use nitinol_runtime::ProcessSystem;
    /// # async fn wiring() {
    /// # let ps = ProcessSystem::new().await;
    /// # let store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    /// // `with_event_store` does not exist: the stores come from a provider
    /// let system = EventSourceSystem::builder(ps).with_event_store(store).build();
    /// # }
    /// ```
    pub async fn spawn_aggregate<A: Aggregate>(&self, id: AggregateId) -> AggregateProxy<A> {
        self.resolving_props_on::<A>(id, self.persistence.event_store())
            .spawn(&self.ps)
            .await
    }

    /// Return an [`AggregateProps`] on the journal the provider supplies.
    ///
    /// Use this when additional configuration (e.g., a snapshot persistor) is
    /// needed before spawning.  Its `spawn` resolves exactly as
    /// [`spawn_aggregate`][Self::spawn_aggregate] does.
    pub fn aggregate_props<A: Aggregate>(&self, id: AggregateId) -> AggregateProps<A> {
        self.resolving_props_on::<A>(id, self.persistence.event_store())
    }

    /// A reference to the aggregate `id`, built without awaiting and without
    /// activating anything.
    ///
    /// The aggregate is resolved on the reference's first dispatch, and resolved
    /// again after the activation it reached dies — the same reference the
    /// awaited entry points hand back, obtained where awaiting is not possible: a
    /// `fold` over a decision, a crash-restart factory rebuilding a target from a
    /// stored marker.
    ///
    /// Deferring the activation is what makes that sound.  A reference names the
    /// aggregate, not a process, so building one commits to nothing that could
    /// have become stale by the time it is used.
    pub fn aggregate_proxy<A: Aggregate>(&self, id: AggregateId) -> AggregateProxy<A> {
        self.reference_on::<A>(id, self.persistence.event_store())
    }

    /// The same system, with the provider's type erased.
    ///
    /// What this is for is wiring whose types are fixed before a deployment's
    /// provider is known — a saga's dispatch mapping names the system type it
    /// delivers through when the mapping is written. The erased handle names one
    /// type for every provider.
    ///
    /// It is a handle, not a copy: the same process system, the same aggregate
    /// registry, the same provider and the same snapshot persistor. Resolving an
    /// identity through the erased handle reaches the activation the original
    /// would.
    pub fn erase_persistence(&self) -> EventSourceSystem<Arc<dyn PersistenceProvider>> {
        let own: Arc<P> = Arc::clone(&self.persistence);
        let erased: Arc<dyn PersistenceProvider> = own;
        EventSourceSystem {
            ps: self.ps.clone(),
            registry: Arc::clone(&self.registry),
            persistence: Arc::new(erased),
            snapshot_persistor: Arc::clone(&self.snapshot_persistor),
        }
    }
}

impl<P: SnapshotStoreProvider> EventSourceSystem<P> {
    /// The handle onto this system's snapshot persistor, starting it on the
    /// first call.
    ///
    /// One process per system, shared by every caller and every clone: the
    /// snapshot contract is written for a single consumer serialising its calls,
    /// and two processes over one store would be two consumers.
    ///
    /// # Typestate
    ///
    /// This exists only where the provider supplies a snapshot store. A
    /// provider that supplies none is refused here rather than at the first
    /// save:
    ///
    /// ```compile_fail
    /// # use std::sync::Arc;
    /// # use nitinol_eventsource::system::EventSourceSystem;
    /// # use nitinol_persistence::provider::Persistence;
    /// # use nitinol_persistence::store::{EventStore, InMemoryEventStore};
    /// # use nitinol_runtime::ProcessSystem;
    /// # async fn wiring() {
    /// # let ps = ProcessSystem::new().await;
    /// # let store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    /// // no `with_snapshot_store` — this provider has no snapshot slot
    /// let system = EventSourceSystem::builder(ps)
    ///     .with_persistence(Persistence::new(store))
    ///     .build();
    /// system.snapshot_persistor().await;
    /// # }
    /// ```
    pub async fn snapshot_persistor(&self) -> SnapshotPersistorProxy {
        self.snapshot_persistor
            .get_or_init(|| async { SnapshotPersistor::spawn(&self.ps, &*self.persistence).await })
            .await
            .clone()
    }
}

impl<P: ProjectionCheckpointProvider> EventSourceSystem<P> {
    /// Return a [`ProjectorProps`] over the journal and the projection
    /// checkpoints the provider supplies.
    ///
    /// Both slots are resolved here, so a projector built from a system follows
    /// the same journal its aggregates write to and records its position where
    /// the deployment's other projections record theirs.
    ///
    /// # Typestate
    ///
    /// This exists only where the provider supplies projection checkpoints:
    ///
    /// ```compile_fail
    /// # use std::sync::Arc;
    /// # use nitinol_eventsource::system::EventSourceSystem;
    /// # use nitinol_persistence::provider::Persistence;
    /// # use nitinol_persistence::store::{EventStore, InMemoryEventStore};
    /// # use nitinol_persistence::ProjectionId;
    /// # use nitinol_runtime::ProcessSystem;
    /// # struct ReadModel;
    /// # async fn wiring() {
    /// # let ps = ProcessSystem::new().await;
    /// # let store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    /// // no `with_projection_checkpoints` — this provider has no projection slot
    /// let system = EventSourceSystem::builder(ps)
    ///     .with_persistence(Persistence::new(store))
    ///     .build();
    /// system.projector_props(ProjectionId::new("read-model"), || ReadModel);
    /// # }
    /// ```
    pub fn projector_props<Proj>(
        &self,
        projection_id: ProjectionId,
        producer: impl Fn() -> Proj + Send + Sync + 'static,
    ) -> ProjectorProps<Proj, P::ProjectionCheckpoints>
    where
        Proj: Send + Sync + 'static,
    {
        ProjectorProps::new(projection_id, &*self.persistence, producer)
    }
}
