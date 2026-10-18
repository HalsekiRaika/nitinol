use std::marker::PhantomData;
use std::sync::Arc;

use crate::projection::ProjectionId;
use nitinol_contract::Event;
use nitinol_persistence::provider::ProjectionOffsetSlot;
use nitinol_persistence::store::{DeliveryMode, OffsetScope, OffsetStore};
use nitinol_persistence::AggregateId;
use nitinol_runtime::process::ProcessProxy;
use nitinol_runtime::{Props, SupervisionStrategy};

use crate::durable_stream::{DurableSubscription, SequenceCursor};
use crate::projection::handler::{ConcreteHandler, EventTypeHandler};
use crate::projection::process::{CatchupOrigin, ProjectorProcess};
use crate::projection::projector::Projector;
use crate::projection::tx_provider::{ErasedTxProvider, TxProvider};
use crate::system::{EventSourceSystem, Hosted, Unhosted};

pub struct EventUnset;
pub struct EventSet;
pub struct OriginUnset;
pub struct OriginSet(pub(crate) CatchupOrigin);

/// Wiring for one projection.
///
/// What it carries is which event types to follow, where to catch up from and
/// how to deliver — never the journal or the offset store. Both come from the
/// system the projection is spawned on: `H` is whether one has been named yet,
/// with [`Unhosted`] props taking a system at [`spawn`][Self::spawn] and props
/// from [`EventSourceSystem::projector_props`] carrying it already. The two
/// forms meet on one private path, so a projector follows the same journal and
/// records its position in the same slot either way.
pub struct ProjectorProps<P, Tx = (), E = EventUnset, O = OriginUnset, H = Unhosted> {
    projection_id: ProjectionId,
    delivery_mode: DeliveryMode,
    catchup_origin: O,
    tx_provider: Option<Arc<dyn ErasedTxProvider<Tx> + Send + Sync>>,
    producer: Box<dyn Fn() -> P + Send + Sync>,
    handlers: Vec<Arc<dyn EventTypeHandler<P, Tx>>>,
    host: H,
    _phantom: PhantomData<E>,
}

impl<P> ProjectorProps<P, (), EventUnset, OriginUnset, Unhosted>
where
    P: Send + Sync + 'static,
{
    /// Wiring for the projection `projection_id`.
    ///
    /// Neither the journal it follows nor the slot its position is recorded in
    /// is named here: the system it is spawned on distributes both, and a store
    /// holding no projection offsets is refused at that spawn rather than at
    /// the first save.
    ///
    /// ```compile_fail
    /// # use nitinol_eventsource::ProjectorProps;
    /// # use nitinol_eventsource::ProjectionId;
    /// # use nitinol_persistence::provider::{CustomStore, PersistenceProvider};
    /// # use nitinol_persistence::store::InMemoryEventStore;
    /// # struct ReadModel;
    /// # fn wiring() {
    /// // no `projection_offsets` — this store has no projection slot
    /// let store = CustomStore::builder().eventstore(InMemoryEventStore::default()).build();
    /// let persistence = PersistenceProvider::new(store);
    /// // there is no form of this that takes a supplier
    /// ProjectorProps::new(ProjectionId::new("read-model"), &persistence, || ReadModel);
    /// # }
    /// ```
    pub fn new(
        projection_id: ProjectionId,
        producer: impl Fn() -> P + Send + Sync + 'static,
    ) -> Self {
        Self {
            projection_id,
            delivery_mode: DeliveryMode::AtLeastOnce,
            catchup_origin: OriginUnset,
            tx_provider: None,
            producer: Box::new(producer),
            handlers: Vec::new(),
            host: Unhosted,
            _phantom: PhantomData,
        }
    }
}

impl<P, O, H> ProjectorProps<P, (), EventUnset, O, H>
where
    P: Send + Sync + 'static,
{
    /// Set a transaction provider and automatically configure ExactlyOnce delivery.
    ///
    /// Providing a `TxProvider` is only meaningful with `ExactlyOnce` semantics —
    /// the delivery mode is therefore fixed to `ExactlyOnce` by this call and cannot
    /// be overridden afterwards.  Calling `delivery_mode` after `with_tx_provider`
    /// is a **compile error** (the method is unavailable when `Tx != ()`).
    ///
    /// Whether the offset store the deployment's supplier holds can take part in
    /// that transaction is checked where the store becomes known — at the
    /// [`spawn`][ProjectorProps::spawn], whose bound reads
    /// `St::ProjectionOffsets: OffsetStore<Tx = Tx>`.
    pub fn with_tx_provider<TP>(self, provider: TP) -> ProjectorProps<P, TP::Tx, EventUnset, O, H>
    where
        TP: TxProvider,
    {
        ProjectorProps {
            projection_id: self.projection_id,
            // ExactlyOnce is the only delivery mode that makes sense with a TxProvider.
            delivery_mode: DeliveryMode::ExactlyOnce,
            catchup_origin: self.catchup_origin,
            tx_provider: Some(Arc::new(provider) as Arc<dyn ErasedTxProvider<TP::Tx> + Send + Sync>),
            producer: self.producer,
            handlers: Vec::new(),
            host: self.host,
            _phantom: PhantomData,
        }
    }
}

impl<P, Tx, OldE, O, H> ProjectorProps<P, Tx, OldE, O, H>
where
    P: Send + Sync + 'static,
    Tx: Send + 'static,
{
    /// Subscribe this projection to the event type `E`.
    ///
    /// Which event types a projection follows stays an explicit statement —
    /// that is a design decision about the projection, not something to infer.
    /// What reads them is not: `E` names the codec that wrote it, so no codec
    /// argument appears here and no wiring can pair an event with a format its
    /// writer never used.
    pub fn with_event<E>(mut self) -> ProjectorProps<P, Tx, EventSet, O, H>
    where
        P: Projector<E, Tx>,
        E: Event,
    {
        let handler: Arc<dyn EventTypeHandler<P, Tx>> = Arc::new(ConcreteHandler::<P, E> {
            _phantom: PhantomData,
        });
        self.handlers.push(handler);
        ProjectorProps {
            projection_id: self.projection_id,
            delivery_mode: self.delivery_mode,
            catchup_origin: self.catchup_origin,
            tx_provider: self.tx_provider,
            producer: self.producer,
            handlers: self.handlers,
            host: self.host,
            _phantom: PhantomData,
        }
    }
}

impl<P, OldE, O, H> ProjectorProps<P, (), OldE, O, H>
where
    P: Send + Sync + 'static,
{
    /// Override the delivery mode when no `TxProvider` is configured.
    ///
    /// This method is intentionally unavailable after `with_tx_provider` is
    /// called (`Tx != ()`).  A `TxProvider` always implies `ExactlyOnce`; allowing
    /// the caller to override the mode afterwards would silently break the Tx
    /// wiring (the delivery engine only activates the Tx path when `mode ==
    /// ExactlyOnce`).
    pub fn delivery_mode(mut self, mode: DeliveryMode) -> Self {
        self.delivery_mode = mode;
        self
    }
}

impl<P, Tx, E, H> ProjectorProps<P, Tx, E, OriginUnset, H>
where
    P: Send + Sync + 'static,
    Tx: Send + 'static,
{
    pub fn catchup_from_aggregate(
        self,
        agg_id: AggregateId,
    ) -> ProjectorProps<P, Tx, E, OriginSet, H> {
        ProjectorProps {
            projection_id: self.projection_id,
            delivery_mode: self.delivery_mode,
            catchup_origin: OriginSet(CatchupOrigin::Aggregate(agg_id)),
            tx_provider: self.tx_provider,
            producer: self.producer,
            handlers: self.handlers,
            host: self.host,
            _phantom: PhantomData,
        }
    }

    pub fn catchup_from_global(self) -> ProjectorProps<P, Tx, E, OriginSet, H> {
        ProjectorProps {
            projection_id: self.projection_id,
            delivery_mode: self.delivery_mode,
            catchup_origin: OriginSet(CatchupOrigin::Global),
            tx_provider: self.tx_provider,
            producer: self.producer,
            handlers: self.handlers,
            host: self.host,
            _phantom: PhantomData,
        }
    }
}

impl<P, Tx, H> ProjectorProps<P, Tx, EventSet, OriginSet, H>
where
    P: Send + Sync + 'static,
    Tx: Send + 'static,
{
    /// The one road both entry points take: start this projector on `system`,
    /// from the position its slot records.
    ///
    /// The journal, the offset store and the process system are taken from
    /// `system` here and nowhere else, so both forms start the same projector
    /// from the same recorded position.
    async fn start_on<St>(
        self,
        system: &EventSourceSystem<St>,
    ) -> ProcessProxy<ProjectorProcess<P, St::ProjectionOffsets, Tx>>
    where
        St: ProjectionOffsetSlot,
        St::ProjectionOffsets: OffsetStore<Tx = Tx> + Send + Sync,
    {
        let store = system.persistence().event_store();
        let offset_store = system.persistence().projection_offsets();

        let catchup_origin = self.catchup_origin.0;
        let projection_id = self.projection_id;
        let delivery_mode = self.delivery_mode;
        let handlers = self.handlers;
        let producer = self.producer;
        let tx_provider = self.tx_provider;

        // Built once here: the key this projector's position is filed under is
        // the same for the load below and for every save it makes afterwards.
        let offset_scope = OffsetScope::from(&projection_id);

        let offset = offset_store
            .load(&offset_scope)
            .await
            .expect("offset load must succeed before projector spawn")
            .unwrap_or(0);

        let cursor = match &catchup_origin {
            CatchupOrigin::Aggregate(agg_id) => SequenceCursor::Stream {
                key: agg_id.as_str().to_owned(),
                after: offset,
            },
            CatchupOrigin::Global => SequenceCursor::Global { after: offset },
        };

        let upstream_config = DurableSubscription::<nitinol_persistence::LoadedEvent>::new(Some);

        let runtime_props = Props::new(move || ProjectorProcess {
            projector: producer(),
            projection_id: projection_id.clone(),
            offset_scope: offset_scope.clone(),
            store: store.clone(),
            offset_store: offset_store.clone(),
            delivery_mode,
            catchup_origin: catchup_origin.clone(),
            handlers: handlers.clone(),
            tx_provider: tx_provider.clone(),
            offset_sequence: offset,
            upstream_config: upstream_config.clone(),
            upstream_cursor: cursor.clone(),
        })
        // An event the projector could not apply is reported by returning
        // `Err`, which is also what supervision sees.  Under the runtime's
        // default `Stop` the first such failure would take the projector down
        // and cascade-stop its poller, so the event held for redelivery would
        // never be polled again — the projection would be dead rather than
        // waiting.  `Restart` is no better here: it rebuilds the projector from
        // `producer()`, discarding the state a stateful projector accumulated
        // and the offset it was restored with.
        .with_supervision_strategy(SupervisionStrategy::Resume);

        system.process_system().spawn(runtime_props).await
    }
}

impl<P, Tx> ProjectorProps<P, Tx, EventSet, OriginSet, Unhosted>
where
    P: Send + Sync + 'static,
    Tx: Send + 'static,
{
    /// Spawn the projector on `system`, following the journal it distributes and
    /// recording its position in the projection slot its store holds.
    ///
    /// Both [`with_event`][ProjectorProps::with_event] and one of the
    /// `catchup_from_*` methods must be called before `spawn`.  Calling `spawn`
    /// from an incomplete builder is a **compile error**.
    ///
    /// Calling `spawn` without any `with_event` call (builder is in [`EventUnset`] state):
    ///
    /// ```compile_fail
    /// # use nitinol_eventsource::ProjectorProps;
    /// # use nitinol_eventsource::ProjectionId;
    /// # use nitinol_eventsource::system::EventSourceSystem;
    /// # use nitinol_persistence::provider::PersistenceProvider;
    /// # use nitinol_persistence::store::InMemoryStore;
    /// # use nitinol_runtime::ProcessSystem;
    /// # struct MyProj;
    /// # impl MyProj { fn new() -> Self { MyProj } }
    /// # async fn bad() {
    /// let ps = ProcessSystem::new().await;
    /// let system = EventSourceSystem::builder(ps)
    ///     .with_persistence(PersistenceProvider::new(InMemoryStore::default()))
    ///     .build();
    /// // compile error: spawn() requires EventSet — call with_event() first
    /// ProjectorProps::new(ProjectionId::new("p"), MyProj::new)
    ///     .spawn(&system)
    ///     .await;
    /// # }
    /// ```
    ///
    /// Calling `spawn` after `with_event` but without a `catchup_from_*` call (builder is in
    /// [`OriginUnset`] state):
    ///
    /// ```compile_fail
    /// # use async_trait::async_trait;
    /// # use nitinol_eventsource::{Event, Projector, ProjectorProps, ProjectionContext};
    /// # use nitinol_eventsource::codec::Codec;
    /// # use nitinol_eventsource::ProjectionId;
    /// # use nitinol_eventsource::system::EventSourceSystem;
    /// # use nitinol_persistence::{EventType, Family, TypeName};
    /// # use nitinol_persistence::provider::PersistenceProvider;
    /// # use nitinol_persistence::store::InMemoryStore;
    /// # use nitinol_runtime::ProcessSystem;
    /// # use bytes::Bytes;
    /// # #[derive(Clone, PartialEq, Debug)] struct MyEv;
    /// # struct MyCodec;
    /// # impl Codec<MyEv> for MyCodec {
    /// #     type Error = std::convert::Infallible;
    /// #     fn encode(_: &MyEv) -> Result<Bytes, Self::Error> { Ok(Bytes::new()) }
    /// #     fn decode(_: &[u8]) -> Result<MyEv, Self::Error> { Ok(MyEv) }
    /// # }
    /// # impl Event for MyEv {
    /// #     const EVENT_TYPE: EventType = EventType::new(Family::new(""), TypeName::new("Ev"));
    /// #     type Codec = MyCodec;
    /// # }
    /// # struct MyProj;
    /// # impl MyProj { fn new() -> Self { MyProj } }
    /// # #[async_trait]
    /// # impl Projector<MyEv> for MyProj {
    /// #     type Error = std::convert::Infallible;
    /// #     async fn project(&mut self, _: MyEv, _: &mut ProjectionContext<'_, ()>) -> Result<(), Self::Error> { Ok(()) }
    /// # }
    /// # async fn bad() {
    /// let ps = ProcessSystem::new().await;
    /// let system = EventSourceSystem::builder(ps)
    ///     .with_persistence(PersistenceProvider::new(InMemoryStore::default()))
    ///     .build();
    /// // compile error: spawn() requires OriginSet — call catchup_from_aggregate() or catchup_from_global() first
    /// ProjectorProps::new(ProjectionId::new("p"), MyProj::new)
    ///     .with_event::<MyEv>()
    ///     .spawn(&system)
    ///     .await;
    /// # }
    /// ```
    ///
    /// # Typestate
    ///
    /// A store holding no projection offsets has no `ProjectionOffsets` to
    /// satisfy this bound, so it is a compile error here:
    ///
    /// ```compile_fail
    /// # use nitinol_eventsource::ProjectorProps;
    /// # use nitinol_eventsource::ProjectionId;
    /// # use nitinol_eventsource::system::EventSourceSystem;
    /// # use nitinol_persistence::provider::{CustomStore, PersistenceProvider};
    /// # use nitinol_persistence::store::InMemoryEventStore;
    /// # use nitinol_runtime::ProcessSystem;
    /// # struct ReadModel;
    /// # async fn bad() {
    /// # let ps = ProcessSystem::new().await;
    /// // no `projection_offsets` — this store has no projection slot
    /// let store = CustomStore::builder().eventstore(InMemoryEventStore::default()).build();
    /// let system = EventSourceSystem::builder(ps)
    ///     .with_persistence(PersistenceProvider::new(store))
    ///     .build();
    /// ProjectorProps::new(ProjectionId::new("read-model"), || ReadModel)
    ///     .catchup_from_global()
    ///     .spawn(&system)
    ///     .await;
    /// # }
    /// ```
    ///
    /// And so is a system whose offset store cannot take part in the
    /// transaction the configured [`TxProvider`] hands out — the two are
    /// matched here, where both are known:
    ///
    /// ```compile_fail
    /// # use async_trait::async_trait;
    /// # use nitinol_eventsource::{Event, Projector, ProjectorProps, ProjectionContext, TxProvider};
    /// # use nitinol_eventsource::codec::Codec;
    /// # use nitinol_eventsource::ProjectionId;
    /// # use nitinol_eventsource::system::EventSourceSystem;
    /// # use nitinol_persistence::{EventType, Family, TypeName};
    /// # use nitinol_persistence::provider::PersistenceProvider;
    /// # use nitinol_persistence::store::InMemoryStore;
    /// # use nitinol_runtime::ProcessSystem;
    /// # use bytes::Bytes;
    /// # #[derive(Clone, PartialEq, Debug)] struct MyEv;
    /// # struct MyCodec;
    /// # impl Codec<MyEv> for MyCodec {
    /// #     type Error = std::convert::Infallible;
    /// #     fn encode(_: &MyEv) -> Result<Bytes, Self::Error> { Ok(Bytes::new()) }
    /// #     fn decode(_: &[u8]) -> Result<MyEv, Self::Error> { Ok(MyEv) }
    /// # }
    /// # impl Event for MyEv {
    /// #     const EVENT_TYPE: EventType = EventType::new(Family::new(""), TypeName::new("Ev"));
    /// #     type Codec = MyCodec;
    /// # }
    /// # struct MyTx;
    /// # struct MyTxProvider;
    /// # #[async_trait]
    /// # impl TxProvider for MyTxProvider {
    /// #     type Tx = MyTx;
    /// #     type Error = std::convert::Infallible;
    /// #     async fn begin(&self) -> Result<Self::Tx, Self::Error> { Ok(MyTx) }
    /// #     async fn commit(&self, _: Self::Tx) -> Result<(), Self::Error> { Ok(()) }
    /// #     async fn rollback(&self, _: Self::Tx) -> Result<(), Self::Error> { Ok(()) }
    /// # }
    /// # struct MyProj;
    /// # #[async_trait]
    /// # impl Projector<MyEv, MyTx> for MyProj {
    /// #     type Error = std::convert::Infallible;
    /// #     async fn project(&mut self, _: MyEv, _: &mut ProjectionContext<'_, MyTx>) -> Result<(), Self::Error> { Ok(()) }
    /// # }
    /// # async fn bad() {
    /// # let ps = ProcessSystem::new().await;
    /// // `InMemoryStore`'s projection offsets have `Tx = ()`, not `MyTx`
    /// let system = EventSourceSystem::builder(ps)
    ///     .with_persistence(PersistenceProvider::new(InMemoryStore::default()))
    ///     .build();
    /// ProjectorProps::new(ProjectionId::new("p"), || MyProj)
    ///     .with_tx_provider(MyTxProvider)
    ///     .with_event::<MyEv>()
    ///     .catchup_from_global()
    ///     .spawn(&system)
    ///     .await;
    /// # }
    /// ```
    pub async fn spawn<St>(
        self,
        system: &EventSourceSystem<St>,
    ) -> ProcessProxy<ProjectorProcess<P, St::ProjectionOffsets, Tx>>
    where
        St: ProjectionOffsetSlot,
        St::ProjectionOffsets: OffsetStore<Tx = Tx> + Send + Sync,
    {
        self.start_on(system).await
    }
}

impl<P, Tx, St> ProjectorProps<P, Tx, EventSet, OriginSet, Hosted<St>>
where
    P: Send + Sync + 'static,
    Tx: Send + 'static,
    St: ProjectionOffsetSlot,
    St::ProjectionOffsets: OffsetStore<Tx = Tx> + Send + Sync,
{
    /// Spawn the projector on the system these props came from.
    ///
    /// Nothing is named here: the journal, the projection slot and the process
    /// system are all that system's. The same two completeness requirements
    /// the argument-taking form states hold — `with_event` and one of the
    /// `catchup_from_*` methods — and are compile errors in the same way.
    pub async fn spawn(self) -> ProcessProxy<ProjectorProcess<P, St::ProjectionOffsets, Tx>> {
        let host = self.host.system().clone();
        self.start_on(&host).await
    }
}

impl<P, Tx, E, O> ProjectorProps<P, Tx, E, O, Unhosted> {
    /// Carry `host` as where these props start.
    pub(crate) fn hosted<St>(
        self,
        host: EventSourceSystem<St>,
    ) -> ProjectorProps<P, Tx, E, O, Hosted<St>> {
        ProjectorProps {
            projection_id: self.projection_id,
            delivery_mode: self.delivery_mode,
            catchup_origin: self.catchup_origin,
            tx_provider: self.tx_provider,
            producer: self.producer,
            handlers: self.handlers,
            host: Hosted::new(host),
            _phantom: PhantomData,
        }
    }
}
