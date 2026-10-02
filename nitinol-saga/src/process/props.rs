#[cfg(test)]
use std::collections::HashMap;
use std::sync::Arc;

use nitinol_eventsource::SequenceCursor;
use nitinol_persistence::store::EventStore;
use nitinol_persistence::AggregateId;
use nitinol_runtime::ProcessSystem;

use crate::dead_letter::{self, DeadLetterEvent, DlqChildSpawn, EnqueuePolicy};
use crate::effect::TellIntent;
use crate::id::SagaId;
use crate::process::instance::{SagaInstanceSpec, ScheduleDelivery};
use crate::process::proxy::SagaProxy;
#[cfg(test)]
use crate::process::saga_process::TellState;
use crate::process::saga_process::{
    self, CrashRestartFactory, DecodeFailureRouteFn, OwnedUpstream, SagaProcess,
};
use crate::saga::Saga;
use crate::scheduler::SchedulerProxy;
use nitinol_runtime::process::{Process, ProcessProxy, Receive};

/// Marker: the upstream subscription has not yet been provided.
pub struct SubscriptionUnset;

/// Marker: the upstream subscription has been provided.
pub struct SubscriptionSet<S: Saga> {
    pub(crate) upstream_store: Arc<dyn EventStore>,
    pub(crate) cursor: SequenceCursor,
    pub(crate) _saga: std::marker::PhantomData<fn() -> S>,
}

/// Builder for a saga spawn.
///
/// State parameter:
/// - `Sub` — subscription configuration (`SubscriptionUnset` / `SubscriptionSet<S>`)
///
/// There is no codec stage: `S::Event` and `S::SubscribedEvent` each name the
/// codec that writes and reads them, so a saga has no format of its own to be
/// handed and no way to journal itself in one its event type never named.
pub struct SagaProps<S: Saga, Sub = SubscriptionUnset> {
    saga_id: SagaId,
    store: Arc<dyn EventStore>,
    producer: Arc<dyn Fn() -> S + Send + Sync>,
    subscription: Sub,
    crash_restart_factory: Option<CrashRestartFactory>,
    scheduler: Option<SchedulerProxy>,
    enqueue_policy: Option<Arc<dyn EnqueuePolicy>>,
    dead_letter_subscriber: Option<Arc<dyn DlqChildSpawn<SagaProcess<S>>>>,
    decode_failure_route_fn: Option<DecodeFailureRouteFn>,
    #[cfg(test)]
    initial_tell_states: HashMap<u64, TellState>,
}

impl<S: Saga> SagaProps<S, SubscriptionUnset> {
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

impl<S: Saga> SagaProps<S, SubscriptionUnset> {
    /// Subscribe this saga instance to an upstream `EventStore`.
    ///
    /// At spawn time the saga starts a runtime child `DirectPollerProcess`
    /// (catchup + live, at-least-once).  The poller is cascade-stopped when
    /// the saga stops.
    ///
    /// `cursor` is where the poller starts *at the latest*: a stream cursor is
    /// carried forward to the position the saga's own stream records for that
    /// stream, so a restart does not re-read what it has already consumed.  The
    /// configured position still holds as a floor.
    ///
    /// Which of the delivered events belong to this instance is decided by
    /// [`Saga::correlate`], not here — correlation is domain knowledge carried
    /// by the saga type.
    pub fn with_subscription(
        self,
        upstream_store: Arc<dyn EventStore>,
        cursor: SequenceCursor,
    ) -> SagaProps<S, SubscriptionSet<S>> {
        SagaProps {
            saga_id: self.saga_id,
            store: self.store,
            producer: self.producer,
            subscription: SubscriptionSet {
                upstream_store,
                cursor,
                _saga: std::marker::PhantomData,
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

impl<S: Saga, Sub> SagaProps<S, Sub> {
    /// Register a factory for crash-restart re-dispatch.
    ///
    /// Which pending tells this recovers, which it cannot, and what the factory
    /// is obliged to return are stated with the rest of the delivery contract on
    /// [`SagaEffect::tell`](crate::SagaEffect::tell).
    pub fn with_crash_restart_factory<F>(mut self, factory: F) -> Self
    where
        F: Fn(&[u8]) -> Option<TellIntent> + Send + Sync + 'static,
    {
        self.crash_restart_factory = Some(Arc::new(factory));
        self
    }

    /// Inject the resident [`SchedulerProxy`] — the proxy
    /// `system.spawn_resident::<`[`SchedulerProcess`](crate::SchedulerProcess)`>()`
    /// answers with, converted — so this saga's [`SagaEffect::schedule`] and
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
        self.dead_letter_subscriber = Some(dead_letter::make_dlq_child_spawn::<SagaProcess<S>, P>(
            subscriber,
        ));
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

impl<S: Saga> SagaProps<S, SubscriptionSet<S>> {
    /// Spawn the saga process.
    ///
    /// The saga's upstream subscription is wired as a direct poller child
    /// process — no shared [`nitinol_eventsource::DurableStream`] fan-out
    /// channel is created.  The poller's lifetime is bound to the saga
    /// process; when the saga stops the runtime cascade-stops the poller.
    pub async fn spawn(self, system: &ProcessSystem) -> SagaProxy<S> {
        let spec = SagaInstanceSpec::<S> {
            saga_id: self.saga_id,
            store: self.store,
            producer: self.producer,
            enqueue_policy: self
                .enqueue_policy
                .unwrap_or_else(dead_letter::default_enqueue_policy),
            upstream: Some(OwnedUpstream {
                config: saga_process::upstream_subscription::<S>(self.subscription.upstream_store),
                cursor: self.subscription.cursor,
            }),
            crash_restart_factory: self.crash_restart_factory,
            scheduler: self.scheduler,
            // Nothing fronts this saga, so a deadline's firing is addressed to
            // the process itself; there is no manager to restore it from its
            // own stream.
            delivery: ScheduleDelivery::ToTheInstance,
            decode_failure_route_fn: self.decode_failure_route_fn,
            dlq_child_spawn: self.dead_letter_subscriber,
            #[cfg(test)]
            initial_tell_states: self.initial_tell_states,
        };

        system.spawn(spec.into_props()).await.into()
    }
}
