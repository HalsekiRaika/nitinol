use std::marker::PhantomData;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::Duration;

use crate::projection::ProjectionId;
use nitinol_contract::Event;
use nitinol_persistence::provider::ProjectionOffsetSlot;
use nitinol_persistence::store::{DeliveryMode, OffsetScope, OffsetStore};
use nitinol_persistence::AggregateId;
use nitinol_runtime::process::ProcessProxy;
use nitinol_runtime::{Props, SupervisionStrategy};

use crate::durable_stream::{
    DurableSubscription, SequenceCursor, DEFAULT_BATCH_LIMIT, DEFAULT_POLL_INTERVAL,
};
use crate::projection::handler::{ConcreteHandler, EventTypeHandler};
use crate::projection::process::{CatchupOrigin, ProjectorProcess};
use crate::projection::projector::Projector;
use crate::projection::tx_provider::{ErasedTxProvider, TxProvider};
use crate::system::{EventSourceSystem, Hosted, Unhosted};

/// Marker for props on which [`with_event`][ProjectorProps::with_event] has not
/// been called yet.
pub struct EventUnset;
/// Marker for props on which [`with_event`][ProjectorProps::with_event] has
/// been called.
pub struct EventSet;
/// Marker for props on which neither
/// [`catchup_from_aggregate`][ProjectorProps::catchup_from_aggregate] nor
/// [`catchup_from_global`][ProjectorProps::catchup_from_global] has been called
/// yet.
pub struct OriginUnset;
/// Marker for props on which
/// [`catchup_from_aggregate`][ProjectorProps::catchup_from_aggregate] or
/// [`catchup_from_global`][ProjectorProps::catchup_from_global] has been
/// called.
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
///
/// The projector these props start settles what each read of the journal
/// returned as one batch — one offset save, and under `ExactlyOnce` with a
/// [`TxProvider`] one transaction — of at most
/// [`with_batch_limit`][Self::with_batch_limit] records.
pub struct ProjectorProps<P, Tx = (), E = EventUnset, O = OriginUnset, H = Unhosted> {
    projection_id: ProjectionId,
    delivery_mode: DeliveryMode,
    catchup_origin: O,
    tx_provider: Option<Arc<dyn ErasedTxProvider<Tx> + Send + Sync>>,
    producer: Box<dyn Fn() -> P + Send + Sync>,
    handlers: Vec<Arc<dyn EventTypeHandler<P, Tx>>>,
    poll_interval: Duration,
    batch_limit: NonZeroUsize,
    host: H,
    _phantom: PhantomData<E>,
}

impl<P> ProjectorProps<P, (), EventUnset, OriginUnset, Unhosted>
where
    P: Send + Sync + 'static,
{
    /// Wiring for the projection `projection_id`.
    ///
    /// The journal and the offset slot come from the system it is spawned on, so
    /// a store holding no projection offsets is refused at that spawn rather than
    /// at the first save.
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
            poll_interval: DEFAULT_POLL_INTERVAL,
            batch_limit: DEFAULT_BATCH_LIMIT,
            host: Unhosted,
            _phantom: PhantomData,
        }
    }
}

impl<P, Tx, E, O, H> ProjectorProps<P, Tx, E, O, H> {
    /// Wait `interval` between reads of the journal; defaults to
    /// [`DEFAULT_POLL_INTERVAL`].
    ///
    /// The first read does not wait, and a read that came back with
    /// [`with_batch_limit`][Self::with_batch_limit] records and settled whole is
    /// followed by the next at once, so a backlog is read back to back.
    pub fn with_poll_interval(mut self, interval: Duration) -> Self {
        self.poll_interval = interval;
        self
    }

    /// Read at most `limit` records from the journal at a time; defaults to
    /// [`DEFAULT_BATCH_LIMIT`].
    ///
    /// One read is one batch: one offset save and, under `ExactlyOnce` with a
    /// [`TxProvider`], one transaction, redone whole when a record fails. A store
    /// that returns more than `limit` has all of it applied as one batch.
    pub fn with_batch_limit(mut self, limit: NonZeroUsize) -> Self {
        self.batch_limit = limit;
        self
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
            poll_interval: self.poll_interval,
            batch_limit: self.batch_limit,
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
            poll_interval: self.poll_interval,
            batch_limit: self.batch_limit,
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
    /// Unavailable after `with_tx_provider` (`Tx != ()`): a `TxProvider` fixes the
    /// mode to `ExactlyOnce`, the only mode in which its transaction is used.
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
    /// Follows only the stream of `agg_id`, in its per-stream sequence order.
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
            poll_interval: self.poll_interval,
            batch_limit: self.batch_limit,
            host: self.host,
            _phantom: PhantomData,
        }
    }

    /// Follows every stream in the journal, in global sequence order.
    pub fn catchup_from_global(self) -> ProjectorProps<P, Tx, E, OriginSet, H> {
        ProjectorProps {
            projection_id: self.projection_id,
            delivery_mode: self.delivery_mode,
            catchup_origin: OriginSet(CatchupOrigin::Global),
            tx_provider: self.tx_provider,
            producer: self.producer,
            handlers: self.handlers,
            poll_interval: self.poll_interval,
            batch_limit: self.batch_limit,
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
        let poll_interval = self.poll_interval;
        let batch_limit = self.batch_limit;

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

        let upstream_config = DurableSubscription::<nitinol_persistence::LoadedEvent>::new(Some)
            .with_poll_interval(poll_interval);

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
            batch_limit,
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
    /// Call [`with_event`][ProjectorProps::with_event] and one of the
    /// `catchup_from_*` methods before `spawn`. An incomplete builder, a store
    /// holding no projection offsets, and an offset store whose `Tx` is not the
    /// configured [`TxProvider`]'s are all **compile errors**. Without
    /// `with_event`:
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
    /// # Panics
    ///
    /// Panics if the offset store fails to load the position recorded for this
    /// projection.
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
    /// system are all that system's. The requirements the argument-taking form
    /// states hold here too and are compile errors in the same way.
    ///
    /// # Panics
    ///
    /// Panics if the offset store fails to load the position recorded for this
    /// projection.
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
            poll_interval: self.poll_interval,
            batch_limit: self.batch_limit,
            host: Hosted::new(host),
            _phantom: PhantomData,
        }
    }
}

#[cfg(doctest)]
mod compile_fail_fixtures {
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
    fn spawn_requires_a_catchup_origin() {}

    /// ```compile_fail
    /// # use async_trait::async_trait;
    /// # use nitinol_eventsource::{Event, Projector, ProjectorProps, ProjectionContext};
    /// # use nitinol_eventsource::codec::Codec;
    /// # use nitinol_eventsource::ProjectionId;
    /// # use nitinol_eventsource::system::EventSourceSystem;
    /// # use nitinol_persistence::{EventType, Family, TypeName};
    /// # use nitinol_persistence::provider::{CustomStore, PersistenceProvider};
    /// # use nitinol_persistence::store::InMemoryEventStore;
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
    /// # struct ReadModel;
    /// # #[async_trait]
    /// # impl Projector<MyEv> for ReadModel {
    /// #     type Error = std::convert::Infallible;
    /// #     async fn project(&mut self, _: MyEv, _: &mut ProjectionContext<'_, ()>) -> Result<(), Self::Error> { Ok(()) }
    /// # }
    /// # async fn bad() {
    /// # let ps = ProcessSystem::new().await;
    /// // no `projection_offsets` — this store has no projection slot
    /// let store = CustomStore::builder().eventstore(InMemoryEventStore::default()).build();
    /// let system = EventSourceSystem::builder(ps)
    ///     .with_persistence(PersistenceProvider::new(store))
    ///     .build();
    /// // compile error: unsatisfied trait bound — the store is not a `ProjectionOffsetSlot`
    /// ProjectorProps::new(ProjectionId::new("read-model"), || ReadModel)
    ///     .with_event::<MyEv>()
    ///     .catchup_from_global()
    ///     .spawn(&system)
    ///     .await;
    /// # }
    /// ```
    fn spawn_requires_a_projection_offset_slot() {}

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
    /// #     async fn rollback(&self, _: Self::Tx) {}
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
    /// // compile error: type mismatch — `spawn` requires the offset store's `Tx` to be `MyTx`
    /// ProjectorProps::new(ProjectionId::new("p"), || MyProj)
    ///     .with_tx_provider(MyTxProvider)
    ///     .with_event::<MyEv>()
    ///     .catchup_from_global()
    ///     .spawn(&system)
    ///     .await;
    /// # }
    /// ```
    fn spawn_requires_an_offset_store_in_the_tx() {}
}
