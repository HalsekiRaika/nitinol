use std::future::Future;
use std::marker::PhantomData;
use std::pin::Pin;
use std::sync::Arc;

use nitinol_persistence::store::{CheckpointStore, DeliveryMode, EventStore};
use nitinol_persistence::{AggregateId, ProjectionId};
use nitinol_runtime::process::ProcessProxy;
use nitinol_runtime::{Props, ProcessSystem, Stream};

use crate::event::Event;
use crate::EventCodec;
use crate::projection::envelope::EventEnvelope;
use crate::projection::handler::{ConcreteHandler, EventTypeHandler};
use crate::projection::process::{CatchupOrigin, ProjectorProcess};
use crate::projection::projector::Projector;

/// A boxed, one-shot subscription action executed after the projector process
/// is spawned.  Captures a `ProcessProxy<Stream<EventEnvelope<E>>>` and
/// subscribes the projector proxy to it.
type SubscribeFn<P, Cs> = Box<
    dyn FnOnce(
            ProcessProxy<ProjectorProcess<P, Cs>>,
        ) -> Pin<Box<dyn Future<Output = ()> + Send>>
        + Send,
>;

/// Builder for spawning a `ProjectorProcess<P, Cs>`.
///
/// # Mandatory
/// - `new(projection_id, event_store, checkpoint_store, producer)`
/// - `with_event::<E>(codec)` — at least one event type
/// - `catchup_from_aggregate(id)` or `catchup_from_global()`
/// - `spawn(system)`
///
/// # Optional
/// - `delivery_mode(mode)` (default: `AtLeastOnce`)
/// - `subscribe(stream)` — one call per live stream
pub struct ProjectorProps<P, Cs>
where
    P: Send + Sync + 'static,
    Cs: CheckpointStore + Send + Sync + 'static,
{
    projection_id: ProjectionId,
    event_store: Arc<dyn EventStore>,
    checkpoint_store: Arc<Cs>,
    delivery_mode: DeliveryMode,
    catchup_origin: Option<CatchupOrigin>,
    producer: Box<dyn Fn() -> P + Send + Sync>,
    handlers: Vec<Arc<dyn EventTypeHandler<P, ()>>>,
    subscriptions: Vec<SubscribeFn<P, Cs>>,
}

impl<P, Cs> ProjectorProps<P, Cs>
where
    P: Send + Sync + 'static,
    Cs: CheckpointStore + Send + Sync + 'static,
{
    pub fn new(
        projection_id: ProjectionId,
        event_store: Arc<dyn EventStore>,
        checkpoint_store: Arc<Cs>,
        producer: impl Fn() -> P + Send + Sync + 'static,
    ) -> Self {
        Self {
            projection_id,
            event_store,
            checkpoint_store,
            delivery_mode: DeliveryMode::AtLeastOnce,
            catchup_origin: None,
            producer: Box::new(producer),
            handlers: Vec::new(),
            subscriptions: Vec::new(),
        }
    }

    /// Register a codec for event type `E`.
    ///
    /// During catch-up, each stored event whose `event_type` matches `E::EVENT_TYPE`
    /// is decoded with this codec and dispatched to `P: Projector<E>`.
    pub fn with_event<E>(mut self, codec: Arc<dyn EventCodec<E>>) -> Self
    where
        P: Projector<E>,
        E: Event,
    {
        let handler: Arc<dyn EventTypeHandler<P, ()>> = Arc::new(ConcreteHandler::<P, E> {
            codec,
            _phantom: PhantomData,
        });
        self.handlers.push(handler);
        self
    }

    /// Override the delivery mode (default: `AtLeastOnce`).
    pub fn delivery_mode(mut self, mode: DeliveryMode) -> Self {
        self.delivery_mode = mode;
        self
    }

    /// Catch up from the beginning of a single aggregate's event stream.
    pub fn catchup_from_aggregate(mut self, agg_id: AggregateId) -> Self {
        self.catchup_origin = Some(CatchupOrigin::Aggregate(agg_id));
        self
    }

    /// Catch up from the global event stream (all aggregates, global-sequence order).
    pub fn catchup_from_global(mut self) -> Self {
        self.catchup_origin = Some(CatchupOrigin::Global);
        self
    }

    /// Subscribe the projector process to a typed live stream.
    ///
    /// Each call may use a different event type `E` so long as `P: Projector<E>`.
    /// The subscription is registered after the process is spawned.
    pub fn subscribe<E>(mut self, stream: ProcessProxy<Stream<EventEnvelope<E>>>) -> Self
    where
        P: Projector<E>,
        E: Event,
    {
        // The closure captures `stream` and subscribes the spawned proxy to it.
        // `ProjectorProcess<P, Cs>: Receive<EventEnvelope<E>, Response = ()>` is
        // guaranteed by `P: Projector<E>` (via the blanket impl in process.rs).
        let sub: SubscribeFn<P, Cs> = Box::new(
            move |proxy: ProcessProxy<ProjectorProcess<P, Cs>>| {
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

    /// Spawn the `ProjectorProcess` and return a proxy.
    ///
    /// The catch-up runs inside `on_start` of the spawned process.
    /// Live-stream subscriptions are wired up immediately after spawning.
    pub async fn spawn(
        self,
        system: &ProcessSystem,
    ) -> ProcessProxy<ProjectorProcess<P, Cs>> {
        let catchup_origin = self.catchup_origin.unwrap_or(CatchupOrigin::Global);
        let projection_id = self.projection_id;
        let event_store = self.event_store;
        let checkpoint_store = self.checkpoint_store;
        let delivery_mode = self.delivery_mode;
        let handlers = self.handlers;
        let producer = self.producer;

        let runtime_props = Props::new(move || ProjectorProcess {
            projector: producer(),
            projection_id: projection_id.clone(),
            event_store: Arc::clone(&event_store),
            checkpoint_store: Arc::clone(&checkpoint_store),
            delivery_mode,
            catchup_origin: catchup_origin.clone(),
            handlers: handlers.clone(),
            checkpoint_sequence: 0,
        });

        let proxy = system.spawn(runtime_props).await;

        // Subscribe to each registered live stream.
        for sub_fn in self.subscriptions {
            sub_fn(proxy.clone()).await;
        }

        proxy
    }
}
