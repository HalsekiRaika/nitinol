use std::marker::PhantomData;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use nitinol_persistence::store::{CheckpointStore, DeliveryMode, EventStore};
use nitinol_persistence::{AggregateId, ProjectionId};
use nitinol_runtime::ident::ProcessName;
use nitinol_runtime::process::ProcessProxy;
use nitinol_runtime::{ProcessSystem, Props};

use crate::codec::ErasedCodec;
use crate::durable_stream::{DurableStream, DurableStreamProxy, SequenceCursor};
use crate::event::Event;
use crate::projection::handler::{ConcreteHandler, EventTypeHandler};
use crate::projection::process::{CatchupOrigin, ProjectorProcess};
use crate::projection::projector::Projector;
use crate::projection::tx_provider::{ErasedTxProvider, TxProvider};

pub struct EventUnset;
pub struct EventSet;
pub struct OriginUnset;
pub struct OriginSet(pub(crate) CatchupOrigin);

static UNIQUE_COUNTER: AtomicU64 = AtomicU64::new(0);

pub struct ProjectorProps<P, Cs, Tx = (), E = EventUnset, O = OriginUnset> {
    projection_id: ProjectionId,
    store: Arc<dyn EventStore>,
    checkpoint_store: Arc<Cs>,
    delivery_mode: DeliveryMode,
    catchup_origin: O,
    tx_provider: Option<Arc<dyn ErasedTxProvider<Tx> + Send + Sync>>,
    producer: Box<dyn Fn() -> P + Send + Sync>,
    handlers: Vec<Arc<dyn EventTypeHandler<P, Tx>>>,
    _phantom: PhantomData<E>,
}

impl<P, Cs> ProjectorProps<P, Cs, (), EventUnset, OriginUnset>
where
    P: Send + Sync + 'static,
    Cs: CheckpointStore + Send + Sync + 'static,
{
    pub fn new(
        projection_id: ProjectionId,
        store: Arc<dyn EventStore>,
        checkpoint_store: Arc<Cs>,
        producer: impl Fn() -> P + Send + Sync + 'static,
    ) -> Self {
        Self {
            projection_id,
            store,
            checkpoint_store,
            delivery_mode: DeliveryMode::AtLeastOnce,
            catchup_origin: OriginUnset,
            tx_provider: None,
            producer: Box::new(producer),
            handlers: Vec::new(),
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
    pub fn with_tx_provider<TP>(self, provider: TP) -> ProjectorProps<P, Cs, TP::Tx, EventUnset, O>
    where
        TP: TxProvider,
        Cs: CheckpointStore<Tx = TP::Tx>,
    {
        ProjectorProps {
            projection_id: self.projection_id,
            store: self.store,
            checkpoint_store: self.checkpoint_store,
            // ExactlyOnce is the only delivery mode that makes sense with a TxProvider.
            delivery_mode: DeliveryMode::ExactlyOnce,
            catchup_origin: self.catchup_origin,
            tx_provider: Some(Arc::new(provider) as Arc<dyn ErasedTxProvider<TP::Tx> + Send + Sync>),
            producer: self.producer,
            handlers: Vec::new(),
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
    pub fn with_event<E>(
        mut self,
        codec: Arc<dyn ErasedCodec<E>>,
    ) -> ProjectorProps<P, Cs, Tx, EventSet, O>
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
            store: self.store,
            checkpoint_store: self.checkpoint_store,
            delivery_mode: self.delivery_mode,
            catchup_origin: self.catchup_origin,
            tx_provider: self.tx_provider,
            producer: self.producer,
            handlers: self.handlers,
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

impl<P, Cs, Tx, E> ProjectorProps<P, Cs, Tx, E, OriginUnset>
where
    P: Send + Sync + 'static,
    Cs: CheckpointStore + Send + Sync + 'static,
    Tx: Send + 'static,
{
    pub fn catchup_from_aggregate(
        self,
        agg_id: AggregateId,
    ) -> ProjectorProps<P, Cs, Tx, E, OriginSet> {
        ProjectorProps {
            projection_id: self.projection_id,
            store: self.store,
            checkpoint_store: self.checkpoint_store,
            delivery_mode: self.delivery_mode,
            catchup_origin: OriginSet(CatchupOrigin::Aggregate(agg_id)),
            tx_provider: self.tx_provider,
            producer: self.producer,
            handlers: self.handlers,
            _phantom: PhantomData,
        }
    }

    pub fn catchup_from_global(self) -> ProjectorProps<P, Cs, Tx, E, OriginSet> {
        ProjectorProps {
            projection_id: self.projection_id,
            store: self.store,
            checkpoint_store: self.checkpoint_store,
            delivery_mode: self.delivery_mode,
            catchup_origin: OriginSet(CatchupOrigin::Global),
            tx_provider: self.tx_provider,
            producer: self.producer,
            handlers: self.handlers,
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
    /// Calling `spawn` without any `with_event` call (builder is in [`EventUnset`] state):
    ///
    /// ```compile_fail
    /// # use std::sync::Arc;
    /// # use nitinol_eventsource::ProjectorProps;
    /// # use nitinol_persistence::ProjectionId;
    /// # use nitinol_persistence::store::{EventStore, InMemoryEventStore, InMemoryCheckpointStore};
    /// # use nitinol_runtime::ProcessSystem;
    /// # struct MyProj;
    /// # impl MyProj { fn new() -> Self { MyProj } }
    /// # async fn bad() {
    /// let system = ProcessSystem::new().await;
    /// let store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    /// let cs = Arc::new(InMemoryCheckpointStore::default());
    /// // compile error: spawn() requires EventSet — call with_event() first
    /// ProjectorProps::new(ProjectionId::new("p"), store, cs, MyProj::new)
    ///     .spawn(&system)
    ///     .await;
    /// # }
    /// ```
    ///
    /// Calling `spawn` after `with_event` but without a `catchup_from_*` call (builder is in
    /// [`OriginUnset`] state):
    ///
    /// ```compile_fail
    /// # use std::sync::Arc;
    /// # use async_trait::async_trait;
    /// # use nitinol_eventsource::{Event, Projector, ProjectorProps, ProjectionContext};
    /// # use nitinol_eventsource::codec::Codec;
    /// # use nitinol_persistence::{EventType, ProjectionId};
    /// # use nitinol_persistence::store::{EventStore, InMemoryEventStore, InMemoryCheckpointStore};
    /// # use nitinol_runtime::ProcessSystem;
    /// # use bytes::Bytes;
    /// # #[derive(Clone, PartialEq, Debug)] struct MyEv;
    /// # impl Event for MyEv { const EVENT_TYPE: EventType = EventType::from_str("Ev"); }
    /// # struct MyProj;
    /// # impl MyProj { fn new() -> Self { MyProj } }
    /// # #[async_trait]
    /// # impl Projector<MyEv> for MyProj {
    /// #     type Error = std::convert::Infallible;
    /// #     async fn project(&mut self, _: MyEv, _: &mut ProjectionContext<'_, ()>) -> Result<(), Self::Error> { Ok(()) }
    /// # }
    /// # struct MyCodec;
    /// # impl Codec<MyEv> for MyCodec {
    /// #     type Error = std::convert::Infallible;
    /// #     fn encode(_: &MyEv) -> Result<Bytes, Self::Error> { Ok(Bytes::new()) }
    /// #     fn decode(_: &[u8]) -> Result<MyEv, Self::Error> { Ok(MyEv) }
    /// # }
    /// # async fn bad() {
    /// let system = ProcessSystem::new().await;
    /// let store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    /// let cs = Arc::new(InMemoryCheckpointStore::default());
    /// // compile error: spawn() requires OriginSet — call catchup_from_aggregate() or catchup_from_global() first
    /// ProjectorProps::new(ProjectionId::new("p"), store, cs, MyProj::new)
    ///     .with_event::<MyEv>(Arc::new(MyCodec))
    ///     .spawn(&system)
    ///     .await;
    /// # }
    /// ```
    pub async fn spawn(self, system: &ProcessSystem) -> ProcessProxy<ProjectorProcess<P, Cs, Tx>> {
        let catchup_origin = self.catchup_origin.0;
        let projection_id = self.projection_id;
        let store = self.store;
        let checkpoint_store = self.checkpoint_store;
        let delivery_mode = self.delivery_mode;
        let handlers = self.handlers;
        let producer = self.producer;
        let tx_provider = self.tx_provider;

        let checkpoint = checkpoint_store
            .load(&projection_id)
            .await
            .expect("checkpoint load must succeed before projector spawn")
            .unwrap_or(0);

        let cursor = match &catchup_origin {
            CatchupOrigin::Aggregate(agg_id) => SequenceCursor::Stream {
                key: agg_id.as_str().to_owned(),
                after: checkpoint,
            },
            CatchupOrigin::Global => SequenceCursor::Global { after: checkpoint },
        };

        let topic = ProcessName::new(format!(
            "projection-stream-{}-{}",
            projection_id.as_str(),
            UNIQUE_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));

        let ds_proxy: Arc<DurableStreamProxy<nitinol_persistence::LoadedEvent>> = Arc::new(
            DurableStream::<nitinol_persistence::LoadedEvent>::new(topic, Arc::clone(&store), Some)
                .cursor(cursor.clone())
                .spawn(system)
                .await
                .expect("DurableStream::spawn must succeed for projector"),
        );

        let ds_for_props = Arc::clone(&ds_proxy);
        let runtime_props = Props::new(move || ProjectorProcess {
            projector: producer(),
            projection_id: projection_id.clone(),
            checkpoint_store: Arc::clone(&checkpoint_store),
            delivery_mode,
            catchup_origin: catchup_origin.clone(),
            handlers: handlers.clone(),
            tx_provider: tx_provider.clone(),
            checkpoint_sequence: checkpoint,
            _ds_keepalive: Arc::clone(&ds_for_props),
        });

        let proxy = system.spawn(runtime_props).await;

        ds_proxy
            .subscribe_from(system, proxy.clone(), cursor)
            .await
            .expect("DurableStream::subscribe_from must succeed for projector");

        proxy
    }
}
