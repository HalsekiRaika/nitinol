use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use nitinol_eventsource::codec::ErasedCodec;
use nitinol_eventsource::EventEnvelope;
use nitinol_persistence::store::EventStore;
use nitinol_runtime::process::{ProcessProxy, Stream};
use nitinol_runtime::{ProcessSystem, Props};

use crate::id::SagaId;
use crate::process::proxy::SagaProxy;
use crate::process::saga_process::{RouteFn, SagaProcess};
use crate::saga::Saga;

/// Marker: the event codec has not yet been provided.
pub struct CodecUnset;

/// Marker: the event codec has been provided.
pub struct CodecSet<E> {
    pub(crate) codec: Arc<dyn ErasedCodec<E>>,
}

/// Marker: the upstream subscription has not yet been provided.
pub struct SubscriptionUnset;

/// Marker: the upstream subscription has been provided.
pub struct SubscriptionSet<S: Saga> {
    pub(crate) subscribe_fn: SubscribeFn<S>,
    pub(crate) route_fn: RouteFn<S::SubscribedEvent>,
}

type SubscribeFn<S> = Box<
    dyn FnOnce(ProcessProxy<SagaProcess<S>>) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send,
>;

/// Builder for a saga spawn.
///
/// State parameters:
/// - `C` — codec configuration (`CodecUnset` / `CodecSet<S::Event>`)
/// - `Sub` — subscription configuration (`SubscriptionUnset` / `SubscriptionSet<S>`)
pub struct SagaProps<S: Saga, C = CodecUnset, Sub = SubscriptionUnset> {
    saga_id: SagaId,
    store: Arc<dyn EventStore>,
    producer: Arc<dyn Fn() -> S + Send + Sync>,
    codec: C,
    subscription: Sub,
}

impl<S: Saga> SagaProps<S, CodecUnset, SubscriptionUnset> {
    /// Begin a saga spawn.
    ///
    /// `producer` constructs the saga instance.  Closure form is required so
    /// dependencies that the saga captures (e.g. an `AggregateProxy` to tell)
    /// can be re-acquired if the runtime restarts the process under a
    /// `Restart` supervision strategy.
    pub fn new(
        saga_id: SagaId,
        store: Arc<dyn EventStore>,
        producer: impl Fn() -> S + Send + Sync + 'static,
    ) -> Self {
        Self {
            saga_id,
            store,
            producer: Arc::new(producer),
            codec: CodecUnset,
            subscription: SubscriptionUnset,
        }
    }
}

impl<S: Saga, Sub> SagaProps<S, CodecUnset, Sub> {
    /// Bind the codec used to encode and decode the saga's own events.
    pub fn with_codec(self, codec: Arc<dyn ErasedCodec<S::Event>>) -> SagaProps<S, CodecSet<S::Event>, Sub> {
        SagaProps {
            saga_id: self.saga_id,
            store: self.store,
            producer: self.producer,
            codec: CodecSet { codec },
            subscription: self.subscription,
        }
    }
}

impl<S: Saga, C> SagaProps<S, C, SubscriptionUnset> {
    /// Subscribe this saga instance to an upstream stream of envelopes.
    ///
    /// `route_fn` decides which saga instance an envelope belongs to.  The
    /// runtime drops envelopes whose route maps to a different `SagaId` (or
    /// to `None`).
    pub fn with_subscription<F>(
        self,
        stream: ProcessProxy<Stream<EventEnvelope<S::SubscribedEvent>>>,
        route_fn: F,
    ) -> SagaProps<S, C, SubscriptionSet<S>>
    where
        F: Fn(&S::SubscribedEvent) -> Option<SagaId> + Send + Sync + 'static,
    {
        let route_fn: RouteFn<S::SubscribedEvent> = Arc::new(route_fn);
        let subscribe_fn: SubscribeFn<S> = Box::new(move |proxy| {
            Box::pin(async move {
                if let Err(e) = stream.subscribe(proxy).await {
                    tracing::error!(error = %e, "saga failed to subscribe to upstream stream");
                }
            })
        });
        SagaProps {
            saga_id: self.saga_id,
            store: self.store,
            producer: self.producer,
            codec: self.codec,
            subscription: SubscriptionSet {
                subscribe_fn,
                route_fn,
            },
        }
    }
}

impl<S: Saga> SagaProps<S, CodecSet<S::Event>, SubscriptionSet<S>> {
    /// Spawn the saga process and register it on the upstream stream.
    ///
    /// Both [`with_codec`][Self::with_codec] and
    /// [`with_subscription`][Self::with_subscription] must be called before
    /// `spawn` — calling it earlier is a compile error.
    pub async fn spawn(self, system: &ProcessSystem) -> SagaProxy<S> {
        let saga_id = self.saga_id;
        let store = self.store;
        let producer = self.producer;
        let codec = self.codec.codec;
        let route_fn = self.subscription.route_fn;
        let subscribe_fn = self.subscription.subscribe_fn;

        let saga_id_for_props = saga_id.clone();
        let store_for_props = Arc::clone(&store);
        let codec_for_props = Arc::clone(&codec);
        let route_fn_for_props = Arc::clone(&route_fn);
        let producer_for_props = Arc::clone(&producer);

        let props = Props::new(move || SagaProcess::<S> {
            state: producer_for_props(),
            saga_id: saga_id_for_props.clone(),
            store: Arc::clone(&store_for_props),
            codec: Arc::clone(&codec_for_props),
            route_fn: Arc::clone(&route_fn_for_props),
            sequence: 0,
        });

        let proxy = system.spawn(props).await;
        subscribe_fn(proxy.clone()).await;
        SagaProxy(proxy)
    }
}
