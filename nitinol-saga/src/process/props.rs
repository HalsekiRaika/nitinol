use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use nitinol_eventsource::codec::ErasedCodec;
use nitinol_eventsource::{DurableStream, EventEnvelope, SequenceCursor};
use nitinol_persistence::store::EventStore;
use nitinol_persistence::AggregateId;
use nitinol_runtime::ident::ProcessName;
use nitinol_runtime::{ProcessSystem, Props};

use crate::effect::TellIntent;
use crate::id::SagaId;
use crate::outbox::RetryPolicy;
use crate::process::proxy::SagaProxy;
use crate::process::saga_process::{CrashRestartFactory, RouteFn, SagaProcess};
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
    pub(crate) upstream_store: Arc<dyn EventStore>,
    pub(crate) upstream_codec: Arc<dyn ErasedCodec<S::SubscribedEvent>>,
    pub(crate) cursor: SequenceCursor,
    pub(crate) route_fn: RouteFn<S::SubscribedEvent>,
}

static UNIQUE_COUNTER: AtomicU64 = AtomicU64::new(0);

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
    crash_restart_factory: Option<CrashRestartFactory>,
    /// Pre-seeded `pending_intents` entries injected at construction time.
    /// Always empty in production; populated in tests via
    /// `with_initial_pending_intents`.
    initial_pending_intents: HashMap<u64, TellIntent>,
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
            crash_restart_factory: None,
            initial_pending_intents: HashMap::new(),
        }
    }
}

impl<S: Saga, Sub> SagaProps<S, CodecUnset, Sub> {
    /// Bind the codec used to encode and decode the saga's own events.
    pub fn with_codec(
        self,
        codec: Arc<dyn ErasedCodec<S::Event>>,
    ) -> SagaProps<S, CodecSet<S::Event>, Sub> {
        SagaProps {
            saga_id: self.saga_id,
            store: self.store,
            producer: self.producer,
            codec: CodecSet { codec },
            subscription: self.subscription,
            crash_restart_factory: self.crash_restart_factory,
            initial_pending_intents: self.initial_pending_intents,
        }
    }
}

impl<S: Saga, C> SagaProps<S, C, SubscriptionUnset> {
    /// Subscribe this saga instance to an upstream `EventStore` via an
    /// internal [`DurableStream`] (catchup + live, at-least-once).
    pub fn with_subscription<F>(
        self,
        upstream_store: Arc<dyn EventStore>,
        upstream_codec: Arc<dyn ErasedCodec<S::SubscribedEvent>>,
        cursor: SequenceCursor,
        route_fn: F,
    ) -> SagaProps<S, C, SubscriptionSet<S>>
    where
        F: Fn(&S::SubscribedEvent) -> Option<SagaId> + Send + Sync + 'static,
    {
        let route_fn: RouteFn<S::SubscribedEvent> = Arc::new(route_fn);
        SagaProps {
            saga_id: self.saga_id,
            store: self.store,
            producer: self.producer,
            codec: self.codec,
            subscription: SubscriptionSet {
                upstream_store,
                upstream_codec,
                cursor,
                route_fn,
            },
            crash_restart_factory: self.crash_restart_factory,
            initial_pending_intents: self.initial_pending_intents,
        }
    }
}

impl<S: Saga, C, Sub> SagaProps<S, C, Sub> {
    /// Register a factory for crash-restart re-dispatch.
    pub fn with_crash_restart_factory<F>(mut self, factory: F) -> Self
    where
        F: Fn(&[u8]) -> Option<TellIntent> + Send + Sync + 'static,
    {
        self.crash_restart_factory = Some(Arc::new(factory));
        self
    }

    /// Pre-seed the `pending_intents` registry of the first spawned
    /// `SagaProcess` instance.
    ///
    /// Used in tests to simulate the scenario where a `SagaProcess` already
    /// has in-flight `TellIntent`s at start (e.g. pre-registered for
    /// supervised-restart re-dispatch via the persistent outbox).
    #[cfg(test)]
    pub(crate) fn with_initial_pending_intents(
        mut self,
        intents: HashMap<u64, TellIntent>,
    ) -> Self {
        self.initial_pending_intents = intents;
        self
    }
}

impl<S: Saga> SagaProps<S, CodecSet<S::Event>, SubscriptionSet<S>> {
    /// Spawn the saga process and wire its internal `DurableStream` against
    /// the upstream `EventStore`.
    pub async fn spawn(self, system: &ProcessSystem) -> SagaProxy<S> {
        let saga_id = self.saga_id;
        let store = self.store;
        let producer = self.producer;
        let codec = self.codec.codec;
        let route_fn = self.subscription.route_fn;
        let upstream_store = self.subscription.upstream_store;
        let upstream_codec = self.subscription.upstream_codec;
        let cursor = self.subscription.cursor;

        let codec_for_transform = Arc::clone(&upstream_codec);
        let transform = move |loaded: nitinol_persistence::LoadedEvent| {
            if loaded.event_type != <S::SubscribedEvent as nitinol_eventsource::Event>::EVENT_TYPE {
                return None;
            }
            match codec_for_transform.decode(&loaded.payload) {
                Ok(event) => Some(EventEnvelope {
                    aggregate_id: AggregateId::new(loaded.stream_key),
                    sequence: loaded.sequence,
                    global_sequence: loaded.global_sequence,
                    event,
                }),
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        event_type = ?loaded.event_type,
                        "saga upstream decode failed; skipping event",
                    );
                    None
                }
            }
        };

        let topic = ProcessName::new(format!(
            "saga-upstream-{}-{}",
            saga_id.as_str(),
            UNIQUE_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));

        let ds_proxy = DurableStream::<EventEnvelope<S::SubscribedEvent>>::new(
            topic,
            upstream_store,
            transform,
        )
        .cursor(cursor.clone())
        .spawn(system)
        .await
        .expect("DurableStream::spawn must succeed for saga upstream subscription");

        let saga_id_for_props = saga_id.clone();
        let store_for_props = Arc::clone(&store);
        let codec_for_props = Arc::clone(&codec);
        let route_fn_for_props = Arc::clone(&route_fn);
        let producer_for_props = Arc::clone(&producer);
        let crash_restart_factory_for_props = self.crash_restart_factory;
        let initial_pending_intents_for_props = self.initial_pending_intents;

        let props = Props::new(move || SagaProcess::<S> {
            state: producer_for_props(),
            saga_id: saga_id_for_props.clone(),
            store: Arc::clone(&store_for_props),
            codec: Arc::clone(&codec_for_props),
            route_fn: Arc::clone(&route_fn_for_props),
            sequence: 0,
            retry_policy: RetryPolicy::default(),
            pending_intents: initial_pending_intents_for_props.clone(),
            crash_restart_factory: crash_restart_factory_for_props.clone(),
            failed_tell_ids: Vec::new(),
            pending_end: false,
            pending_executor_count: 0,
        });

        let saga_proxy = system.spawn(props).await;

        ds_proxy
            .subscribe_from(system, saga_proxy.clone(), cursor)
            .await
            .expect("DurableStream::subscribe_from must succeed for saga upstream subscription");

        // `ds_proxy` is dropped at the end of this scope. The DurableStream's
        // shared poller is idle for the saga upstream wiring (no topic
        // subscribers), and the DirectPollerProcess spawned by
        // `subscribe_from` watches the saga process directly — so the
        // saga's lifetime drives the upstream poller's lifetime, and no
        // explicit keepalive Arc is needed inside `SagaProcess`.
        saga_proxy.into()
    }
}
