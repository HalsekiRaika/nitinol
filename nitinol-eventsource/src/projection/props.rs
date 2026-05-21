use std::future::Future;
use std::marker::PhantomData;
use std::pin::Pin;
use std::sync::Arc;

use nitinol_persistence::store::{CheckpointStore, DeliveryMode};
use nitinol_persistence::{AggregateId, ProjectionId};
use nitinol_runtime::process::ProcessProxy;
use nitinol_runtime::{Props, ProcessSystem, Stream};

use crate::codec::ErasedCodec;
use crate::event::Event;
use crate::process::persistence::EventPersistorProxy;
use crate::projection::envelope::EventEnvelope;
use crate::projection::handler::{ConcreteHandler, EventTypeHandler};
use crate::projection::process::{CatchupOrigin, ProjectorProcess};
use crate::projection::projector::Projector;
use crate::projection::tx_provider::{ErasedTxProvider, TxProvider};

pub struct EventUnset;
pub struct EventSet;
pub struct OriginUnset;
pub struct OriginSet(pub(crate) CatchupOrigin);

type SubscribeFn<P, Cs, Tx> = Box<
    dyn FnOnce(
            ProcessProxy<ProjectorProcess<P, Cs, Tx>>,
        ) -> Pin<Box<dyn Future<Output = ()> + Send>>
        + Send,
>;

pub struct ProjectorProps<P, Cs, Tx = (), E = EventUnset, O = OriginUnset> {
    projection_id: ProjectionId,
    event_ref: EventPersistorProxy,
    checkpoint_store: Arc<Cs>,
    delivery_mode: DeliveryMode,
    catchup_origin: O,
    tx_provider: Option<Arc<dyn ErasedTxProvider<Tx> + Send + Sync>>,
    producer: Box<dyn Fn() -> P + Send + Sync>,
    handlers: Vec<Arc<dyn EventTypeHandler<P, Tx>>>,
    subscriptions: Vec<SubscribeFn<P, Cs, Tx>>,
    _phantom: PhantomData<E>,
}

impl<P, Cs> ProjectorProps<P, Cs, (), EventUnset, OriginUnset>
where
    P: Send + Sync + 'static,
    Cs: CheckpointStore + Send + Sync + 'static,
{
    pub fn new(
        projection_id: ProjectionId,
        event_ref: EventPersistorProxy,
        checkpoint_store: Arc<Cs>,
        producer: impl Fn() -> P + Send + Sync + 'static,
    ) -> Self {
        Self {
            projection_id,
            event_ref,
            checkpoint_store,
            delivery_mode: DeliveryMode::AtLeastOnce,
            catchup_origin: OriginUnset,
            tx_provider: None,
            producer: Box::new(producer),
            handlers: Vec::new(),
            subscriptions: Vec::new(),
            _phantom: PhantomData,
        }
    }
}

impl<P, Cs, O> ProjectorProps<P, Cs, (), EventUnset, O>
where
    P: Send + Sync + 'static,
    Cs: CheckpointStore + Send + Sync + 'static,
{
    /// Set a transaction provider and automatically configure ExactlyOnce delivery.
    ///
    /// Providing a `TxProvider` is only meaningful with `ExactlyOnce` semantics —
    /// the delivery mode is therefore fixed to `ExactlyOnce` by this call and cannot
    /// be overridden afterwards.  Calling `delivery_mode` after `with_tx_provider`
    /// is a **compile error** (the method is unavailable when `Tx != ()`).
    ///
    /// ### Cannot override delivery mode after `with_tx_provider`
    ///
    /// ```compile_fail
    /// # use std::sync::Arc;
    /// # use async_trait::async_trait;
    /// # use nitinol_eventsource::{ProjectorProps, EventPersistorProxy, TxProvider};
    /// # use nitinol_persistence::store::{CheckpointStore, DeliveryMode};
    /// # use nitinol_persistence::error::CheckpointError;
    /// # use nitinol_persistence::ProjectionId;
    /// struct MyTx;
    /// struct MyTxProvider;
    /// #[async_trait]
    /// impl TxProvider for MyTxProvider {
    ///     type Tx = MyTx;
    ///     type Error = std::convert::Infallible;
    ///     async fn begin(&self) -> Result<MyTx, Self::Error> { Ok(MyTx) }
    ///     async fn commit(&self, _: MyTx) -> Result<(), Self::Error> { Ok(()) }
    ///     async fn rollback(&self, _: MyTx) {}
    /// }
    /// struct MyCheckpointStore;
    /// #[async_trait]
    /// impl CheckpointStore for MyCheckpointStore {
    ///     type Tx = MyTx;
    ///     async fn load(&self, _: &ProjectionId) -> Result<Option<u64>, CheckpointError> { Ok(None) }
    ///     async fn save(&self, _: &ProjectionId, _: u64, _: Option<&mut MyTx>) -> Result<(), CheckpointError> { Ok(()) }
    /// }
    /// # async fn _test(event_ref: EventPersistorProxy) {
    /// // ERROR: `delivery_mode` is not available once Tx != ()
    /// let _ = ProjectorProps::new(
    ///     ProjectionId::new("demo"),
    ///     event_ref,
    ///     Arc::new(MyCheckpointStore),
    ///     || (),
    /// )
    /// .with_tx_provider(MyTxProvider)
    /// .delivery_mode(DeliveryMode::AtLeastOnce); // compile error here
    /// # }
    /// ```
    pub fn with_tx_provider<TP>(self, provider: TP) -> ProjectorProps<P, Cs, TP::Tx, EventUnset, O>
    where
        TP: TxProvider,
        Cs: CheckpointStore<Tx = TP::Tx>,
    {
        ProjectorProps {
            projection_id: self.projection_id,
            event_ref: self.event_ref,
            checkpoint_store: self.checkpoint_store,
            // ExactlyOnce is the only delivery mode that makes sense with a TxProvider.
            delivery_mode: DeliveryMode::ExactlyOnce,
            catchup_origin: self.catchup_origin,
            tx_provider: Some(Arc::new(provider) as Arc<dyn ErasedTxProvider<TP::Tx> + Send + Sync>),
            producer: self.producer,
            handlers: Vec::new(),
            subscriptions: Vec::new(),
            _phantom: PhantomData,
        }
    }
}

impl<P, Cs, Tx, OldE, O> ProjectorProps<P, Cs, Tx, OldE, O>
where
    P: Send + Sync + 'static,
    Cs: CheckpointStore + Send + Sync + 'static,
    Tx: Send + 'static,
{
    pub fn with_event<E>(mut self, codec: Arc<dyn ErasedCodec<E>>) -> ProjectorProps<P, Cs, Tx, EventSet, O>
    where
        P: Projector<E, Tx>,
        E: Event,
    {
        let handler: Arc<dyn EventTypeHandler<P, Tx>> = Arc::new(ConcreteHandler::<P, E> {
            codec,
            _phantom: PhantomData,
        });
        self.handlers.push(handler);
        ProjectorProps {
            projection_id: self.projection_id,
            event_ref: self.event_ref,
            checkpoint_store: self.checkpoint_store,
            delivery_mode: self.delivery_mode,
            catchup_origin: self.catchup_origin,
            tx_provider: self.tx_provider,
            producer: self.producer,
            handlers: self.handlers,
            subscriptions: self.subscriptions,
            _phantom: PhantomData,
        }
    }
}

impl<P, Cs, OldE, O> ProjectorProps<P, Cs, (), OldE, O>
where
    P: Send + Sync + 'static,
    Cs: CheckpointStore + Send + Sync + 'static,
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

impl<P, Cs, Tx, O> ProjectorProps<P, Cs, Tx, EventSet, O>
where
    P: Send + Sync + 'static,
    Cs: CheckpointStore + Send + Sync + 'static,
    Tx: Send + 'static,
{
    pub fn subscribe<E>(mut self, stream: ProcessProxy<Stream<EventEnvelope<E>>>) -> Self
    where
        P: Projector<E, Tx>,
        E: Event,
        ProjectorProcess<P, Cs, Tx>: nitinol_runtime::process::Receive<EventEnvelope<E>, Response = ()>,
    {
        let sub: SubscribeFn<P, Cs, Tx> = Box::new(
            move |proxy: ProcessProxy<ProjectorProcess<P, Cs, Tx>>| {
                Box::pin(async move {
                    if let Err(e) = stream.subscribe(proxy).await {
                        tracing::error!(error = %e, "failed to subscribe to live stream");
                    }
                })
            },
        );
        self.subscriptions.push(sub);
        self
    }
}

impl<P, Cs, Tx, E> ProjectorProps<P, Cs, Tx, E, OriginUnset>
where
    P: Send + Sync + 'static,
    Cs: CheckpointStore + Send + Sync + 'static,
    Tx: Send + 'static,
{
    pub fn catchup_from_aggregate(self, agg_id: AggregateId) -> ProjectorProps<P, Cs, Tx, E, OriginSet> {
        ProjectorProps {
            projection_id: self.projection_id,
            event_ref: self.event_ref,
            checkpoint_store: self.checkpoint_store,
            delivery_mode: self.delivery_mode,
            catchup_origin: OriginSet(CatchupOrigin::Aggregate(agg_id)),
            tx_provider: self.tx_provider,
            producer: self.producer,
            handlers: self.handlers,
            subscriptions: self.subscriptions,
            _phantom: PhantomData,
        }
    }

    pub fn catchup_from_global(self) -> ProjectorProps<P, Cs, Tx, E, OriginSet> {
        ProjectorProps {
            projection_id: self.projection_id,
            event_ref: self.event_ref,
            checkpoint_store: self.checkpoint_store,
            delivery_mode: self.delivery_mode,
            catchup_origin: OriginSet(CatchupOrigin::Global),
            tx_provider: self.tx_provider,
            producer: self.producer,
            handlers: self.handlers,
            subscriptions: self.subscriptions,
            _phantom: PhantomData,
        }
    }
}

impl<P, Cs, Tx> ProjectorProps<P, Cs, Tx, EventSet, OriginSet>
where
    P: Send + Sync + 'static,
    Cs: CheckpointStore<Tx = Tx> + Send + Sync + 'static,
    Tx: Send + 'static,
{
    /// Spawn the projector process.
    ///
    /// Both [`with_event`][ProjectorProps::with_event] and one of the
    /// `catchup_from_*` methods must be called before `spawn`.  Calling `spawn`
    /// from an incomplete builder is a **compile error**.
    ///
    /// ### Missing `with_event`
    ///
    /// ```compile_fail
    /// # use std::sync::Arc;
    /// # use nitinol_eventsource::{ProjectorProps, EventPersistorProxy};
    /// # use nitinol_persistence::store::InMemoryCheckpointStore;
    /// # use nitinol_persistence::ProjectionId;
    /// # async fn _test(system: nitinol_runtime::ProcessSystem, event_ref: EventPersistorProxy) {
    /// # let cs = Arc::new(InMemoryCheckpointStore::default());
    /// // ERROR: spawn() not available on ProjectorProps<_, _, _, EventUnset, _>
    /// ProjectorProps::new(
    ///     ProjectionId::new("no-event"),
    ///     event_ref,
    ///     cs,
    ///     || (),
    /// )
    /// .catchup_from_global()
    /// .spawn(&system)
    /// .await;
    /// # }
    /// ```
    ///
    /// ### Missing `catchup_from_*`
    ///
    /// ```compile_fail
    /// # use std::sync::Arc;
    /// # use async_trait::async_trait;
    /// # use bytes::Bytes;
    /// # use nitinol_eventsource::{ProjectorProps, EventPersistorProxy, Event, Projector, ProjectionContext};
    /// # use nitinol_eventsource::codec::Codec;
    /// # use nitinol_persistence::store::InMemoryCheckpointStore;
    /// # use nitinol_persistence::{ProjectionId, EventType};
    /// # #[derive(Clone)] struct DummyEvent;
    /// # impl Event for DummyEvent { const EVENT_TYPE: EventType = EventType::from_str("d.e"); }
    /// # struct DummyCodec;
    /// # impl Codec<DummyEvent> for DummyCodec {
    /// #     type Error = std::convert::Infallible;
    /// #     fn encode(_: &DummyEvent) -> Result<Bytes, Self::Error> { Ok(Bytes::new()) }
    /// #     fn decode(_: &[u8]) -> Result<DummyEvent, Self::Error> { Ok(DummyEvent) }
    /// # }
    /// # struct DummyProjector;
    /// # #[async_trait]
    /// # impl Projector<DummyEvent> for DummyProjector {
    /// #     type Error = std::convert::Infallible;
    /// #     async fn project(&mut self, _: DummyEvent, _: &mut ProjectionContext<'_, ()>) -> Result<(), Self::Error> { Ok(()) }
    /// # }
    /// # async fn _test(system: nitinol_runtime::ProcessSystem, event_ref: EventPersistorProxy) {
    /// # let cs = Arc::new(InMemoryCheckpointStore::default());
    /// // ERROR: spawn() not available on ProjectorProps<_, _, _, EventSet, OriginUnset>
    /// ProjectorProps::new(
    ///     ProjectionId::new("no-origin"),
    ///     event_ref,
    ///     cs,
    ///     || DummyProjector,
    /// )
    /// .with_event::<DummyEvent>(Arc::new(DummyCodec))
    /// .spawn(&system)
    /// .await;
    /// # }
    /// ```
    pub async fn spawn(
        self,
        system: &ProcessSystem,
    ) -> ProcessProxy<ProjectorProcess<P, Cs, Tx>> {
        let catchup_origin = self.catchup_origin.0;
        let projection_id = self.projection_id;
        let event_ref = self.event_ref;
        let checkpoint_store = self.checkpoint_store;
        let delivery_mode = self.delivery_mode;
        let handlers = self.handlers;
        let producer = self.producer;
        let tx_provider = self.tx_provider;

        let runtime_props = Props::new(move || ProjectorProcess {
            projector: producer(),
            projection_id: projection_id.clone(),
            event_ref: event_ref.clone(),
            checkpoint_store: Arc::clone(&checkpoint_store),
            delivery_mode,
            catchup_origin: catchup_origin.clone(),
            handlers: handlers.clone(),
            tx_provider: tx_provider.clone(),
            checkpoint_sequence: 0,
        });

        let proxy = system.spawn(runtime_props).await;

        for sub_fn in self.subscriptions {
            sub_fn(proxy.clone()).await;
        }

        proxy
    }
}
