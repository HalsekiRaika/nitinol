use std::collections::HashMap;
use std::sync::Arc;

use nitinol_eventsource::codec::ErasedCodec;
use nitinol_eventsource::{DurableSubscription, SequenceCursor};
use nitinol_persistence::store::EventStore;
use nitinol_persistence::AggregateId;
use nitinol_runtime::{ProcessSystem, Props};

use crate::dead_letter::{
    make_dlq_child_spawn, DeadLetterEvent, DlqChildSpawn, EnqueueAll, EnqueuePolicy,
};
use crate::effect::TellIntent;
use crate::id::SagaId;
use crate::outbox::RetryPolicy;
use crate::process::proxy::SagaProxy;
use crate::process::saga_process::{
    CrashRestartFactory, DecodeFailureRouteFn, Lifecycle, SagaProcess, TellState, UpstreamMessage,
};
use crate::saga::Saga;
use crate::scheduler::SchedulerProxy;
use nitinol_runtime::process::{Process, ProcessProxy, Receive};

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
}

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
    scheduler: Option<SchedulerProxy>,
    enqueue_policy: Option<Arc<dyn EnqueuePolicy>>,
    dead_letter_subscriber: Option<Arc<dyn DlqChildSpawn<SagaProcess<S>>>>,
    decode_failure_route_fn: Option<DecodeFailureRouteFn>,
    #[cfg(test)]
    initial_tell_states: HashMap<u64, TellState>,
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
            scheduler: None,
            enqueue_policy: None,
            dead_letter_subscriber: None,
            decode_failure_route_fn: None,
            #[cfg(test)]
            initial_tell_states: HashMap::new(),
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
            scheduler: self.scheduler,
            enqueue_policy: self.enqueue_policy,
            dead_letter_subscriber: self.dead_letter_subscriber,
            decode_failure_route_fn: self.decode_failure_route_fn,
            #[cfg(test)]
            initial_tell_states: self.initial_tell_states,
        }
    }
}

impl<S: Saga, C> SagaProps<S, C, SubscriptionUnset> {
    /// Subscribe this saga instance to an upstream `EventStore`.
    ///
    /// At spawn time the saga starts a runtime child `DirectPollerProcess`
    /// (catchup + live, at-least-once).  The poller is cascade-stopped when
    /// the saga stops.
    ///
    /// Which of the delivered events belong to this instance is decided by
    /// [`Saga::correlate`], not here — correlation is domain knowledge carried
    /// by the saga type.
    pub fn with_subscription(
        self,
        upstream_store: Arc<dyn EventStore>,
        upstream_codec: Arc<dyn ErasedCodec<S::SubscribedEvent>>,
        cursor: SequenceCursor,
    ) -> SagaProps<S, C, SubscriptionSet<S>> {
        SagaProps {
            saga_id: self.saga_id,
            store: self.store,
            producer: self.producer,
            codec: self.codec,
            subscription: SubscriptionSet {
                upstream_store,
                upstream_codec,
                cursor,
            },
            crash_restart_factory: self.crash_restart_factory,
            scheduler: self.scheduler,
            enqueue_policy: self.enqueue_policy,
            dead_letter_subscriber: self.dead_letter_subscriber,
            decode_failure_route_fn: self.decode_failure_route_fn,
            #[cfg(test)]
            initial_tell_states: self.initial_tell_states,
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

    /// Inject the resident [`SchedulerProxy`] (from
    /// [`crate::spawn_scheduler`]) so this saga's [`SagaEffect::schedule`] and
    /// [`SagaEffect::cancel_schedule`] effects drive real timers.
    ///
    /// Without it, schedule markers are still persisted but no timer fires until
    /// a scheduler-equipped incarnation replays them.
    ///
    /// [`SagaEffect::schedule`]: crate::SagaEffect::schedule
    /// [`SagaEffect::cancel_schedule`]: crate::SagaEffect::cancel_schedule
    pub fn with_scheduler(mut self, scheduler: SchedulerProxy) -> Self {
        self.scheduler = Some(scheduler);
        self
    }

    /// Override the [`EnqueuePolicy`] that filters which saga failures reach the
    /// DLQ.  Without it the default enqueues every failure kind.
    pub fn with_enqueue_policy(mut self, policy: Arc<dyn EnqueuePolicy>) -> Self {
        self.enqueue_policy = Some(policy);
        self
    }

    /// Register a dead-letter subscriber process.
    ///
    /// When the saga enqueues a [`DeadLetterEvent`] on its own stream, the
    /// subscriber `P` receives it over a [`nitinol_eventsource::DurableSubscription`]
    /// so it catches up from the EventStore even if it was down at enqueue time.
    /// `P`'s type is erased so it never surfaces as a `SagaProps` type
    /// parameter.
    ///
    /// The DLQ direct-poller is started as a **child** of the saga from
    /// `SagaProcess::on_start`: when the saga stops the
    /// runtime cascade-stops the poller automatically, releasing the subscription.
    pub fn with_dead_letter_subscriber<P>(mut self, subscriber: ProcessProxy<P>) -> Self
    where
        P: Process + Receive<DeadLetterEvent, Response = ()> + 'static,
    {
        self.dead_letter_subscriber = Some(make_dlq_child_spawn::<SagaProcess<S>, P>(subscriber));
        self
    }

    /// Override the decode-failure routing behaviour.
    ///
    /// When two or more saga instances subscribe to the same upstream event
    /// type, a corrupt event that cannot be decoded is forwarded to every
    /// subscriber as `UpstreamMessage::DecodeFailed`.  Without a route
    /// function every subscriber records a DLQ entry.  Providing this function
    /// lets the saga decide which instance is the intended recipient:
    /// - Return `Some(saga_id)` to accept the failure (DLQ recorded only when
    ///   `saga_id == self.saga_id`).
    /// - Return `None` to decline the failure (no DLQ recorded for this saga).
    ///
    /// Without this call the legacy behaviour applies: every `DecodeFailed` is
    /// recorded against this saga regardless of other subscribers.
    ///
    /// # Why this stays on the builder while correlation lives on `Saga`
    ///
    /// [`Saga::correlate`] derives a process instance's identity from a typed
    /// domain event.  A decode failure has no typed event — this function is
    /// handed only the upstream stream key and sequence, so it decides *which
    /// subscriber owns a corrupt wire record*, which is routing rather than
    /// correlation.  Keeping it here also keeps the setting per-instance: two
    /// instances of the same saga type subscribed to one upstream stream can
    /// attribute the same corrupt record differently, which a static associated
    /// function on the type could not express.
    pub fn with_decode_failure_route<F>(mut self, route: F) -> Self
    where
        F: Fn(&AggregateId, u64) -> Option<SagaId> + Send + Sync + 'static,
    {
        self.decode_failure_route_fn = Some(Arc::new(route));
        self
    }

    #[cfg(test)]
    pub(crate) fn with_initial_tell_states(mut self, tell_states: HashMap<u64, TellState>) -> Self {
        self.initial_tell_states = tell_states;
        self
    }
}

impl<S: Saga> SagaProps<S, CodecSet<S::Event>, SubscriptionSet<S>> {
    /// Spawn the saga process.
    ///
    /// The saga's upstream subscription is wired as a direct poller child
    /// process — no shared [`nitinol_eventsource::DurableStream`] fan-out
    /// channel is created.  The poller's lifetime is bound to the saga
    /// process; when the saga stops the runtime cascade-stops the poller.
    pub async fn spawn(self, system: &ProcessSystem) -> SagaProxy<S> {
        let saga_id = self.saga_id;
        let store = self.store;
        let producer = self.producer;
        let codec = self.codec.codec;
        let enqueue_policy: Arc<dyn EnqueuePolicy> =
            self.enqueue_policy.unwrap_or_else(|| Arc::new(EnqueueAll));
        let dead_letter_subscriber = self.dead_letter_subscriber;
        let upstream_store = self.subscription.upstream_store;
        let upstream_codec = self.subscription.upstream_codec;
        let cursor = self.subscription.cursor;

        let codec_for_transform = Arc::clone(&upstream_codec);
        let transform = move |loaded: nitinol_persistence::LoadedEvent| {
            if loaded.event_type.type_key()
                != <S::SubscribedEvent as nitinol_eventsource::Event>::EVENT_TYPE.type_key()
            {
                return None;
            }
            // Capture the raw payload before decoding so the draining branch can
            // preserve it in `SagaFailure::EndedSagaReceivedMessage`.
            let raw_payload = loaded.payload.clone();
            let aggregate_id = AggregateId::new(loaded.stream_key);
            let sequence = loaded.sequence;
            match codec_for_transform.decode(&loaded.payload) {
                Ok(event) => Some(UpstreamMessage::Decoded {
                    aggregate_id,
                    sequence,
                    event,
                    raw_payload,
                }),
                Err(e) => {
                    // Decoding is not retried: the failure is forwarded to
                    // SagaProcess as `UpstreamMessage::DecodeFailed`, and
                    // whether it is recorded as a `DeadLetterEvent` is decided
                    // there by `decode_failure_route_fn`.
                    tracing::warn!(
                        error = %e,
                        event_type = ?loaded.event_type,
                        "saga upstream decode failed; forwarding as DecodeFailed",
                    );
                    Some(UpstreamMessage::DecodeFailed {
                        aggregate_id,
                        sequence,
                        error: e.to_string(),
                    })
                }
            }
        };

        let upstream_config = DurableSubscription::<UpstreamMessage<S::SubscribedEvent>>::new(
            upstream_store,
            transform,
        );

        let saga_id_for_props = saga_id.clone();
        let store_for_props = Arc::clone(&store);
        let codec_for_props = Arc::clone(&codec);
        let producer_for_props = Arc::clone(&producer);
        let crash_restart_factory_for_props = self.crash_restart_factory;
        let scheduler_for_props = self.scheduler;
        let enqueue_policy_for_props = Arc::clone(&enqueue_policy);
        let decode_failure_route_fn_for_props = self.decode_failure_route_fn;
        let dlq_child_spawn_for_props = dead_letter_subscriber;
        #[cfg(test)]
        let initial_tell_states_for_props = self.initial_tell_states;
        let upstream_config_for_props = upstream_config;
        let cursor_for_props = cursor;

        let props = Props::new(move || {
            #[cfg(not(test))]
            let tell_states: HashMap<u64, TellState> = HashMap::new();
            #[cfg(test)]
            let tell_states: HashMap<u64, TellState> = initial_tell_states_for_props.clone();
            SagaProcess::<S> {
                state: producer_for_props(),
                saga_id: saga_id_for_props.clone(),
                store: Arc::clone(&store_for_props),
                codec: Arc::clone(&codec_for_props),
                decode_failure_route_fn: decode_failure_route_fn_for_props.clone(),
                sequence: 0,
                retry_policy: RetryPolicy::default(),
                tell_states,
                crash_restart_factory: crash_restart_factory_for_props.clone(),
                lifecycle: Lifecycle::Running,
                upstream_config: upstream_config_for_props.clone(),
                upstream_cursor: cursor_for_props.clone(),
                scheduler: scheduler_for_props.clone(),
                enqueue_policy: Arc::clone(&enqueue_policy_for_props),
                // DLQ child spawner is passed via Arc so it can be cloned into
                // each restart incarnation (Props::new is Fn, not FnOnce).
                dlq_child_spawn: dlq_child_spawn_for_props.clone(),
            }
        });

        system.spawn(props).await.into()
    }
}
