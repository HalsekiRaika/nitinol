//! The system that hosts aggregates, durable streams and projectors.

mod contingent;

use std::sync::Arc;

use crate::projection::ProjectionId;
use nitinol_contract::Aggregate;
use nitinol_persistence::provider::{
    PersistenceProvider, ProjectionOffsetSlot, SnapshotSlot, Store,
};
use nitinol_persistence::store::Album;
use nitinol_persistence::{AggregateId, LoadedEvent};
use nitinol_runtime::ident::ProcessName;
use nitinol_runtime::ProcessSystem;
use tokio::sync::OnceCell;

use crate::durable_stream::{CursorUnset, DurableStream};
use crate::process::resolve::{AggregateRegistry, ResolveHandle};
use crate::process::{RecoverOn, SnapshotPersistor, SnapshotPersistorProxy};
use crate::projection::ProjectorProps;
use crate::{AggregateProps, AggregateProxy};

pub use self::contingent::ContingentSystem;

/// Marker type indicating no persistence supplier has been supplied yet.
///
/// A builder in this state has no [`build`][EventSourceSystemBuilder::build],
/// so there is no way to reach a system whose stores nobody named.
pub struct PersistenceUnset;

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
/// [`build`][EventSourceSystemBuilder::build]. There is no codec stage: every
/// event type names the codec that writes it, so a system has no format of its
/// own to bind.
///
/// # Examples
///
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
    /// - The [`EventStore`](nitinol_persistence::store::EventStore) is
    ///   stream-keyed, so the one journal holds every aggregate's stream (and
    ///   every saga's journal) side by side, each under its own key.
    /// - Placing a stream elsewhere takes a second supplier and a second system
    ///   built on it. There is no per-spawn override: every entry point, and
    ///   every `spawn` that takes a system, resolves the one supplier it carries.
    /// - One supplier is not one transaction: nothing is atomic across
    ///   destinations. The clauses on [`Store`] state what it guarantees.
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
            }),
        }
    }
}

/// Binds a [`ProcessSystem`] to the store a deployment's records live in.
///
/// - `St` is that store. The slots it holds decide which entry points exist,
///   so a slot the store does not hold is a compile error at the wiring that
///   needs it rather than a failure at the first save. The [`ContingentSystem`]
///   from [`to_contingent`][Self::to_contingent] carries the snapshot slot as a
///   value instead.
/// - Each entry point has a props-first twin ([`AggregateProps::new`],
///   [`ProjectorProps::new`], [`DurableStream::new`]) whose `spawn` takes a
///   system and resolves the same slots, registry and activation.
/// - Aggregate entry points resolve rather than spawn: a node has at most one
///   activation per identifier, and concurrent resolves get the same reference.
///   Across a cluster, duplicate activations are normal and the event store's
///   OCC keeps them safe, so never write code that needs single activation. A
///   bare [`tell`](crate::AggregateProxy::tell) is not at-most-once: a side
///   effect that must be cluster-safe is a persisted record, such as the
///   events of a [`Decision`](crate::Decision).
/// - A clone names the same system (registry, [`ProcessSystem`], supplier and
///   snapshot persistor), so share it by cloning rather than with an `Arc`.
///   Nothing the system holds holds it back, so there is no strong cycle.
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
}

/// The node state a system shares with its clones and with the contingent
/// systems taken from it.
///
/// Cheap-clone for the reason [`ProcessSystem`] is, and separate from
/// [`Inner`] for one reason: none of it depends on the store type, so a
/// [`ContingentSystem`] holds this same node rather than a second one. That is
/// what makes the activation registry one table for the typed system and the
/// contingent view of it both — and the snapshot persistor one process.
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
    /// Shared across clones and across contingent systems, because the snapshot
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
    /// a contingent system carries the very album that supplier's store
    /// answered with. So the process the first caller starts is over the store
    /// every later caller would have named.
    async fn snapshot_persistor(&self, snapshots: Album) -> SnapshotPersistorProxy {
        self.inner
            .snapshot_persistor
            .get_or_init(|| async { SnapshotPersistor::spawn(&self.inner.ps, snapshots).await })
            .await
            .clone()
    }

    /// What an activation of a `Recover = Latest` aggregate resolves this
    /// node's one persistor through, once it is running.
    ///
    /// `snapshots` is the asking system's own snapshot store, taken by the
    /// caller from wherever its type says it is: the slot of a typed store, or
    /// the value a contingent system carries.
    fn snapshot_source(&self, snapshots: Album) -> SnapshotSource {
        SnapshotSource {
            node: self.clone(),
            snapshots,
        }
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
/// A clone is another name for the one instance: the same supplier and the same
/// node.
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
    /// The aggregate's stream lives under `id` in the store's journal; other
    /// aggregates, and any saga journalling through this system, are tenants of
    /// the same journal under their own keys. An aggregate whose
    /// [`Recover`](Aggregate::Recover) is [`Latest`](nitinol_contract::Latest)
    /// needs a store with a [`SnapshotSlot`]: one without is a compile error here
    /// rather than an activation that silently replays from the beginning.
    ///
    /// The store always comes from the system's supplier, and a builder never
    /// handed one has no `build`, so this is a **compile error**:
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

    /// A view of this system that names no store type.
    ///
    /// It is the same node, not a copy: the same process system, aggregate
    /// registry, journal and snapshot persistor, so resolving an identity
    /// through it reaches the activation this system would. The snapshot slot
    /// travels as a value (the store's album, or nothing), which
    /// [`ContingentSystem::snapshot_store`] answers with. A value cannot satisfy
    /// a bound, so the view has no snapshot persistor, no props and no supplier;
    /// what that means for an aggregate whose [`Recover`](Aggregate::Recover) is
    /// [`Latest`](nitinol_contract::Latest) is stated on [`ContingentSystem`].
    pub fn to_contingent(&self) -> ContingentSystem {
        ContingentSystem::new(
            self.inner.node.clone(),
            self.inner.persistence.event_store(),
            self.inner.persistence.store().snapshot_slot(),
        )
    }
}

impl<St: SnapshotSlot> EventSourceSystem<St> {
    /// The handle onto this system's snapshot persistor, starting it on the
    /// first call.
    ///
    /// One process per system, shared by every caller and every clone: the
    /// snapshot contract is written for a single consumer serialising its calls.
    /// This exists only where the store holds a snapshot store; one that holds
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
        self.inner
            .node
            .snapshot_source(self.inner.persistence.snapshot_store())
    }
}

impl<St: ProjectionOffsetSlot> EventSourceSystem<St> {
    /// Return a [`ProjectorProps`] over the journal and the projection
    /// offsets the store holds, carrying this system as where it starts.
    ///
    /// A projector built from a system follows the same journal its aggregates
    /// write to and records its position where the deployment's other
    /// projections record theirs. This exists only where the store holds
    /// projection offsets:
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

#[cfg(doctest)]
mod compile_fail_fixtures {
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
    fn spawn_aggregate_latest_needs_a_snapshot_slot() {}
}
