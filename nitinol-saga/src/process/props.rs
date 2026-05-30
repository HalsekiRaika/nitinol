use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use nitinol_eventsource::codec::ErasedCodec;
use nitinol_eventsource::{DurableStream, EventEnvelope, SequenceCursor};
use nitinol_persistence::store::EventStore;
use nitinol_persistence::AggregateId;
use nitinol_runtime::ident::ProcessName;
use nitinol_runtime::{ProcessSystem, Props};
use tokio::sync::Mutex;

use crate::effect::TellIntent;
use crate::id::SagaId;
use crate::outbox::RetryPolicy;
use crate::process::pending_intents::PendingIntents;
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
    pending_intents: PendingIntents,
    crash_restart_factory: Option<CrashRestartFactory>,
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
            pending_intents: PendingIntents::new(),
            crash_restart_factory: None,
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
            pending_intents: self.pending_intents,
            crash_restart_factory: self.crash_restart_factory,
        }
    }
}

impl<S: Saga, C> SagaProps<S, C, SubscriptionUnset> {
    /// Subscribe this saga instance to an upstream `EventStore` via an
    /// internal [`DurableStream`] (catchup + live, at-least-once).
    ///
    /// `upstream_store` is the event store whose events drive the saga,
    /// `upstream_codec` decodes the persisted bytes into `S::SubscribedEvent`,
    /// `cursor` is the initial resume point, and `route_fn` decides which
    /// saga instance an event belongs to.  The runtime drops events whose
    /// route maps to a different `SagaId` (or to `None`).
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
            pending_intents: self.pending_intents,
            crash_restart_factory: self.crash_restart_factory,
        }
    }
}

impl<S: Saga, C, Sub> SagaProps<S, C, Sub> {
    /// Provide an external [`PendingIntents`] registry.
    ///
    /// By default [`spawn`][SagaProps::spawn] creates a fresh, saga-local
    /// registry.  Providing one here enables sharing the registry across
    /// multiple spawns of the same saga — used in crate-internal tests of
    /// supervised restart behaviour.
    #[cfg(test)]
    pub(crate) fn with_pending_intents(mut self, pending_intents: PendingIntents) -> Self {
        self.pending_intents = pending_intents;
        self
    }

    /// Register a factory for crash-restart re-dispatch.
    ///
    /// When the saga process starts after a full OS-process crash, the
    /// in-memory [`crate::process::pending_intents::PendingIntents`] registry
    /// is gone.  This factory is called with the crash-restart bytes stored in
    /// each pending `TellRequested` outbox marker (supplied at intent
    /// construction time via [`TellIntent::new_with_crash_restart`]) and must
    /// return a fresh [`TellIntent`] that re-sends the same command to the
    /// same target.
    ///
    /// If the factory returns `None` for a given payload (e.g. the intent type
    /// is not recognised), a synthetic `TellFailed` is appended to the saga
    /// stream so the outbox reaches a consistent terminal state, and a warning
    /// is emitted.
    ///
    /// `TellRequested` markers written without crash-restart bytes (via
    /// [`TellIntent::new`]) are never passed to this factory; they fall through
    /// to the synthetic `TellFailed` path.
    pub fn with_crash_restart_factory<F>(mut self, factory: F) -> Self
    where
        F: Fn(&[u8]) -> Option<TellIntent> + Send + Sync + 'static,
    {
        self.crash_restart_factory = Some(Arc::new(factory));
        self
    }
}

impl<S: Saga> SagaProps<S, CodecSet<S::Event>, SubscriptionSet<S>> {
    /// Spawn the saga process and wire its internal `DurableStream` against
    /// the upstream `EventStore`.
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
        let upstream_store = self.subscription.upstream_store;
        let upstream_codec = self.subscription.upstream_codec;
        let cursor = self.subscription.cursor;

        // Build the upstream DurableStream first so its keepalive Arc can be
        // moved into SagaProcess.  Tying the lifetime to the process (not to
        // the external SagaProxy handle) ensures the poller is never stopped
        // while the saga is still alive.
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

        let ds_proxy = Arc::new(
            DurableStream::<EventEnvelope<S::SubscribedEvent>>::new(
                topic,
                upstream_store,
                transform,
            )
            .cursor(cursor.clone())
            .spawn(system)
            .await
            .expect("DurableStream::spawn must succeed for saga upstream subscription"),
        );

        let saga_id_for_props = saga_id.clone();
        let store_for_props = Arc::clone(&store);
        let codec_for_props = Arc::clone(&codec);
        let route_fn_for_props = Arc::clone(&route_fn);
        let producer_for_props = Arc::clone(&producer);
        let ds_keepalive_for_props = Arc::clone(&ds_proxy);
        let pending_intents_for_props = self.pending_intents;
        let crash_restart_factory_for_props = self.crash_restart_factory;

        let props = Props::new(move || SagaProcess::<S> {
            state: producer_for_props(),
            saga_id: saga_id_for_props.clone(),
            store: Arc::clone(&store_for_props),
            codec: Arc::clone(&codec_for_props),
            route_fn: Arc::clone(&route_fn_for_props),
            sequence: Arc::new(Mutex::new(0)),
            retry_policy: RetryPolicy::default(),
            pending_intents: pending_intents_for_props.clone(),
            crash_restart_factory: crash_restart_factory_for_props.clone(),
            failed_tell_ids: Arc::new(Mutex::new(Vec::new())),
            _ds_keepalive: Arc::clone(&ds_keepalive_for_props),
        });

        let saga_proxy = system.spawn(props).await;

        ds_proxy
            .subscribe_from(system, saga_proxy.clone(), cursor)
            .await
            .expect("DurableStream::subscribe_from must succeed for saga upstream subscription");

        SagaProxy::new(saga_proxy)
    }
}
