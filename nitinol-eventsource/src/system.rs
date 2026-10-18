use std::sync::{Arc, OnceLock};

use crate::projection::ProjectionId;
use nitinol_contract::Aggregate;
use nitinol_persistence::provider::{
    PersistenceProvider, ProjectionOffsetSlot, SnapshotSlot, Store,
};
use nitinol_persistence::store::{Album, Journal};
use nitinol_persistence::{AggregateId, LoadedEvent};
use nitinol_runtime::ident::ProcessName;
use nitinol_runtime::ProcessSystem;
use tokio::sync::OnceCell;

use crate::durable_stream::{CursorUnset, DurableStream};
use crate::process::resolve::{AggregateRegistry, ResolveHandle};
use crate::process::{RecoverOn, SnapshotPersistor, SnapshotPersistorProxy};
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

/// Props that have not named the system they start on yet, so `spawn` takes
/// one.
///
/// This is the props-first form of every entry point: the wiring is assembled
/// on its own and the system it is spawned on supplies the stores. The
/// system-first form is [`Hosted`], which carries that system already.
pub struct Unhosted;

/// Props that carry the system they came from, so the spawn needs nothing
/// re-injected.
///
/// Every entry point on [`EventSourceSystem`] produces props in this state.
/// `St` is the system's own store type and is carried rather than erased, so a
/// slot the store does not hold is refused at the `spawn` of these props the
/// same way it is refused at the `spawn` of an [`Unhosted`] one.
pub struct Hosted<St>(EventSourceSystem<St>);

impl<St> Hosted<St> {
    /// Carry `host` as where the props holding this start.
    ///
    /// Public because the saga entry points that produce hosted props live in
    /// `nitinol-saga` — the dependency arrow points `saga -> eventsource`, so
    /// `host` is an extension trait there rather than an inherent method here,
    /// and it needs the same state this crate's own entry points put their
    /// props in.
    pub fn new(host: EventSourceSystem<St>) -> Self {
        Self(host)
    }

    /// The system these props were taken from.
    pub fn system(&self) -> &EventSourceSystem<St> {
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
    /// second supplier, assembled and named as such, and a second system built
    /// on it. There is no per-spawn override and no props that carry a store of
    /// their own: every entry point below, and every `spawn` that takes a
    /// system, resolves the one supplier that system carries — so a deployment
    /// cannot half-move a stream and leave two activations writing to two
    /// worlds.
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
            inner: Arc::new(Inner {
                persistence: self.persistence,
                node: Node::new(self.ps),
                erased: OnceLock::new(),
            }),
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
/// | the journal ([`Store`]) | [`spawn_aggregate`][Self::spawn_aggregate] / [`aggregate_props`][Self::aggregate_props] / [`aggregate_proxy`][Self::aggregate_proxy] / [`durable_stream`][Self::durable_stream] |
/// | a snapshot store ([`SnapshotSlot`]) | [`snapshot_persistor`][Self::snapshot_persistor], and spawning an aggregate whose [`Recover`](Aggregate::Recover) is [`Latest`](nitinol_contract::Latest) |
/// | projection offsets ([`ProjectionOffsetSlot`]) | [`projector_props`][Self::projector_props] |
///
/// A slot the store does not hold is a **compile error** at the wiring that
/// needs it, rather than a failure at the first save — which is the whole of
/// why the store is a type parameter here.
///
/// # Two ways to say one wiring
///
/// Every entry point above has a props-first twin that names no store either —
/// [`AggregateProps::new`], [`ProjectorProps::new`], [`DurableStream::new`] —
/// whose `spawn` takes a system and resolves the very same slots. The two forms
/// meet on one private path per props, so which one a deployment writes is a
/// matter of where the wiring reads better and nothing else: the registry, the
/// journal and the activation are the same either way.
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
/// There is no second kind of start to reach for: props built directly resolve
/// through this same registry when they are spawned on a system, so one
/// identity has one activation however its wiring was written.
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
/// A system holds the one instance behind it: the supplier it was built with,
/// the node state every handle onto it shares, and — once asked for — the
/// erased handle it answers with. Nothing on the other side of those holds a
/// system back. The registry keeps process proxies, a process proxy keeps a pid
/// and a channel, and a persistor keeps a snapshot store, so there is no strong
/// cycle to leak: a handle lives as long as the processes holding it, and is
/// released when they stop.
pub struct EventSourceSystem<St = PersistenceUnset> {
    inner: Arc<Inner<St>>,
}

/// The instance every handle onto one system shares.
struct Inner<St> {
    /// Held by value: a [`Store`] is distributed by being cloned and owes the
    /// identity of the records its clones reach, so a supplier over one is
    /// already another name for the same store. Wrapping it here would prove
    /// that property a second time and leave the sharing in the holder rather
    /// than in the store.
    persistence: PersistenceProvider<St>,
    node: Node,
    /// The handle this system erases to, built on the first
    /// [`erase_store`][EventSourceSystem::erase_store] and named by every call
    /// after it.
    ///
    /// Erasing is on the hosted spawn path — every entry point carries one into
    /// the props it hands back — so the journal is taken out of the supplier
    /// once per system rather than once per spawn.
    erased: OnceLock<SystemHandle>,
}

/// The node state a system shares with its clones and with the handle its store
/// type is erased from.
///
/// Cheap-clone for the reason [`ProcessSystem`] is, and separate from
/// [`Inner`] for one reason: none of it depends on the store type, so the
/// erased handle is a second [`Inner`] over the same node rather than a second
/// node. That is what makes the activation registry one table for the typed
/// system and the erased handle both — and the snapshot persistor one process.
#[derive(Clone)]
struct Node {
    inner: Arc<NodeInner>,
}

struct NodeInner {
    /// A handle rather than an owned instance: a reference re-resolves its
    /// aggregate long after the entry point that produced it returned, and needs
    /// a system to activate on.
    ps: ProcessSystem,
    registry: AggregateRegistry,
    /// The one snapshot persistor process a system distributes.
    ///
    /// Shared across clones and across the erased handle, because the snapshot
    /// contract is written for a single consumer serialising its calls: a second
    /// process over one store would be two.
    snapshot_persistor: OnceCell<SnapshotPersistorProxy>,
}

impl Node {
    fn new(ps: ProcessSystem) -> Self {
        Self {
            inner: Arc::new(NodeInner {
                ps,
                registry: AggregateRegistry::new(),
                snapshot_persistor: OnceCell::new(),
            }),
        }
    }

    fn process_system(&self) -> &ProcessSystem {
        &self.inner.ps
    }

    /// What a reference needs to resolve, and re-resolve, an aggregate on this
    /// node.
    fn resolve_handle(&self) -> ResolveHandle {
        ResolveHandle::new(self.inner.registry.clone(), self.inner.ps.clone())
    }

    /// This node's snapshot persistor, starting it on the first call.
    ///
    /// `snapshots` is the asking system's own snapshot store, and it is the same
    /// one every time: a node is built with one supplier, clones carry it, and
    /// the erased handle has no snapshot slot to ask through. So the process the
    /// first caller starts is over the store every later caller would have
    /// named.
    async fn snapshot_persistor(&self, snapshots: Album) -> SnapshotPersistorProxy {
        self.inner
            .snapshot_persistor
            .get_or_init(|| async { SnapshotPersistor::spawn(&self.inner.ps, snapshots).await })
            .await
            .clone()
    }
}

/// What an activation that recovers from a snapshot resolves its persistor
/// through.
///
/// An activation is built synchronously — [`EventSourceSystem::aggregate_proxy`]
/// hands back a reference without awaiting anything — while starting the
/// persistor is not, so the ingredients travel and the resolve happens in
/// `on_start`. It is the node's own memoised cell either way, so an activation
/// and a caller of
/// [`snapshot_persistor`][EventSourceSystem::snapshot_persistor] reach one
/// process.
///
/// The store type is already gone here: a snapshot store is an [`Album`], so
/// nothing an activation holds depends on which deployment assembled it.
#[derive(Clone)]
pub(crate) struct SnapshotSource {
    node: Node,
    snapshots: Album,
}

impl SnapshotSource {
    pub(crate) async fn resolve(&self) -> SnapshotPersistorProxy {
        self.node.snapshot_persistor(self.snapshots.clone()).await
    }
}

/// Written out rather than derived: `derive` would require `St: Clone`, and a
/// builder's marker is not a store and has no clone to offer — so an assembly's
/// choice of handle types would decide whether its system can be shared.
///
/// A clone is another name for the one instance: the same supplier, the same
/// node, and the same erased handle.
impl<St> Clone for EventSourceSystem<St> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
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
        self.inner.node.process_system()
    }

    /// The supplier this system was built with.
    ///
    /// Wiring that takes a store as a value — an upstream subscription, a saga's
    /// own journal — resolves it from here rather than keeping a second handle
    /// alongside the system.
    pub fn persistence(&self) -> &PersistenceProvider<St> {
        &self.inner.persistence
    }

    /// What a reference needs to resolve, and re-resolve, its aggregate here.
    pub(crate) fn resolve_handle(&self) -> ResolveHandle {
        self.inner.node.resolve_handle()
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
    ///
    /// An aggregate whose type says it recovers from the newest snapshot asks
    /// this system for a snapshot store, so one whose store holds none is a
    /// **compile error** here rather than an activation that silently replays
    /// from the beginning:
    ///
    /// ```compile_fail
    /// # use bytes::Bytes;
    /// # use nitinol_eventsource::codec::Codec;
    /// # use nitinol_eventsource::system::EventSourceSystem;
    /// # use nitinol_eventsource::{Aggregate, Event, Latest, Snapshotable};
    /// # use nitinol_persistence::provider::{CustomStore, PersistenceProvider};
    /// # use nitinol_persistence::store::InMemoryEventStore;
    /// # use nitinol_persistence::{AggregateId, EventType, Family, TypeName};
    /// # use nitinol_runtime::ProcessSystem;
    /// # struct DocCodec;
    /// # #[derive(Clone)] struct Counted;
    /// # impl Codec<Counted> for DocCodec {
    /// #     type Error = std::convert::Infallible;
    /// #     fn encode(_: &Counted) -> Result<Bytes, Self::Error> { Ok(Bytes::new()) }
    /// #     fn decode(_: &[u8]) -> Result<Counted, Self::Error> { Ok(Counted) }
    /// # }
    /// # impl Codec<u64> for DocCodec {
    /// #     type Error = std::convert::Infallible;
    /// #     fn encode(_: &u64) -> Result<Bytes, Self::Error> { Ok(Bytes::new()) }
    /// #     fn decode(_: &[u8]) -> Result<u64, Self::Error> { Ok(0) }
    /// # }
    /// # impl Event for Counted {
    /// #     const EVENT_TYPE: EventType = EventType::new(Family::new("doc"), TypeName::new("Counted"));
    /// #     type Codec = DocCodec;
    /// # }
    /// # #[derive(Default)] struct Counter { value: u64 }
    /// # impl Aggregate for Counter {
    /// #     type Event = Counted;
    /// #     type Recover = Latest;
    /// #     fn apply(&mut self, _: Counted) { self.value += 1; }
    /// # }
    /// # impl Snapshotable for Counter {
    /// #     type Snapshot = u64;
    /// #     type Codec = DocCodec;
    /// #     fn capture(&self) -> u64 { self.value }
    /// #     fn recover(snapshot: u64) -> Self { Self { value: snapshot } }
    /// # }
    /// # async fn wiring() {
    /// # let ps = ProcessSystem::new().await;
    /// // no `snapshot` — this store has no snapshot slot
    /// let store = CustomStore::builder().eventstore(InMemoryEventStore::default()).build();
    /// let system = EventSourceSystem::builder(ps)
    ///     .with_persistence(PersistenceProvider::new(store))
    ///     .build();
    /// system.spawn_aggregate::<Counter>(AggregateId::new("counter")).await;
    /// # }
    /// ```
    pub async fn spawn_aggregate<A>(&self, id: AggregateId) -> AggregateProxy<A>
    where
        A: Aggregate,
        A::Recover: RecoverOn<A, St>,
    {
        self.aggregate_props::<A>(id).spawn().await
    }

    /// Return an [`AggregateProps`] carrying this system as where it starts.
    ///
    /// Nothing is handed over here and nothing is left to configure: the
    /// journal, the registry and — for an aggregate that recovers from one —
    /// the snapshot persistor are all taken from this system at the spawn. Its
    /// `spawn` takes no argument and resolves exactly as
    /// [`spawn_aggregate`][Self::spawn_aggregate] does.
    pub fn aggregate_props<A: Aggregate>(&self, id: AggregateId) -> AggregateProps<A, Hosted<St>> {
        AggregateProps::<A>::new(id).hosted(self.clone())
    }

    /// Return a [`DurableStream`] over the journal the store holds, carrying
    /// this system as where it starts.
    ///
    /// The props-first form of the same wiring is
    /// [`DurableStream::new`], which names no journal either and takes this
    /// system at its `spawn`.
    pub fn durable_stream<T, F>(
        &self,
        topic: ProcessName,
        transform: F,
    ) -> DurableStream<T, CursorUnset, Hosted<St>>
    where
        T: Send + Sync + 'static,
        F: Fn(LoadedEvent) -> Option<T> + Send + Sync + 'static,
    {
        DurableStream::<T>::new(topic, transform).hosted(self.clone())
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
    pub fn aggregate_proxy<A>(&self, id: AggregateId) -> AggregateProxy<A>
    where
        A: Aggregate,
        A::Recover: RecoverOn<A, St>,
    {
        self.aggregate_props::<A>(id).into_reference()
    }

    /// The same system, with the store's type erased.
    ///
    /// It is a handle, not a copy: the same process system, the same aggregate
    /// registry, the same journal and the same snapshot persistor. Resolving an
    /// identity through the erased handle reaches the activation the original
    /// would.
    ///
    /// One handle per system, built here on the first call and cloned by every
    /// call after it — including the ones the hosted entry points make on the
    /// caller's behalf, so a spawn takes nothing out of the supplier.
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
    ///
    /// The same applies to an aggregate whose [`Recover`](Aggregate::Recover) is
    /// [`Latest`](nitinol_contract::Latest): it asks its system for a snapshot
    /// store, and a handle that lost the type saying there is one cannot answer.
    /// Such an aggregate is resolved through the typed system it was built on;
    /// wiring written against the erased handle — a saga's dispatch mapping,
    /// say — addresses aggregates that replay their streams.
    pub fn erase_store(&self) -> SystemHandle {
        self.inner
            .erased
            .get_or_init(|| EventSourceSystem {
                inner: Arc::new(Inner {
                    persistence: PersistenceProvider::new(self.inner.persistence.event_store()),
                    node: self.inner.node.clone(),
                    erased: OnceLock::new(),
                }),
            })
            .clone()
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
        self.snapshot_source().resolve().await
    }

    /// What an activation of a `Recover = Latest` aggregate resolves this
    /// system's one persistor through, once it is running.
    pub(crate) fn snapshot_source(&self) -> SnapshotSource {
        SnapshotSource {
            node: self.inner.node.clone(),
            snapshots: self.inner.persistence.snapshot_store(),
        }
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
        (),
        crate::projection::EventUnset,
        crate::projection::OriginUnset,
        Hosted<St>,
    >
    where
        Proj: Send + Sync + 'static,
    {
        ProjectorProps::new(projection_id, producer).hosted(self.clone())
    }
}
