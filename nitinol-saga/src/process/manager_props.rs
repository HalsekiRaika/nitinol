use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use nitinol_eventsource::codec::ErasedCodec;
use nitinol_eventsource::SequenceCursor;
use nitinol_persistence::store::EventStore;
use nitinol_persistence::AggregateId;
use nitinol_runtime::process::{Process, ProcessProxy, Receive, SupervisionStrategy};
use nitinol_runtime::{IdleTimeout, ProcessSystem, Props};

use crate::dead_letter::{
    default_enqueue_policy, make_dlq_child_spawn, DeadLetterEvent, DlqChildSpawn, EnqueuePolicy,
};
use crate::effect::TellIntent;
use crate::id::SagaId;
use crate::process::manager::SagaManagerProcess;
use crate::process::props::{CodecSet, CodecUnset, SubscriptionSet, SubscriptionUnset};
use crate::process::proxy::SagaManagerProxy;
use crate::process::saga_process::{
    upstream_subscription, CrashRestartFactory, DecodeFailureRouteFn, SagaProcess,
};
use crate::saga::Saga;
use crate::scheduler::SchedulerProxy;

/// Builder for the saga instance manager.
///
/// Where [`SagaProps`](crate::SagaProps) spawns one saga bound to one
/// [`SagaId`](crate::SagaId), this spawns a manager that holds a single
/// upstream subscription and derives the instance for each record from
/// [`Saga::correlate`]: an id with no resident instance is spawned and replayed
/// on the spot, an id already resident is handed the record directly.
///
/// The state parameters mirror [`SagaProps`](crate::SagaProps):
/// - `C` — codec configuration (`CodecUnset` / `CodecSet<S::Event>`)
/// - `Sub` — subscription configuration (`SubscriptionUnset` / `SubscriptionSet<S>`)
///
/// # Relationship to a single resident saga
///
/// A [`Saga::correlate`] that always answers the same id reduces the manager to
/// exactly one instance, which behaves as the standalone builder's saga does —
/// same replay, same effect interpretation, same outbox ordering — **provided
/// the same settings are carried over**.  Every instance-level setting
/// [`SagaProps`](crate::SagaProps) accepts has a counterpart here, which the
/// manager hands to each instance it spawns:
///
/// | On [`SagaProps`](crate::SagaProps) | Here |
/// |---|---|
/// | [`with_scheduler`](crate::SagaProps::with_scheduler) | [`with_scheduler`](Self::with_scheduler) |
/// | [`with_enqueue_policy`](crate::SagaProps::with_enqueue_policy) | [`with_enqueue_policy`](Self::with_enqueue_policy) |
/// | [`with_dead_letter_subscriber`](crate::SagaProps::with_dead_letter_subscriber) | [`with_dead_letter_subscriber`](Self::with_dead_letter_subscriber) |
/// | [`with_crash_restart_factory`](crate::SagaProps::with_crash_restart_factory) | [`with_crash_restart_factory`](Self::with_crash_restart_factory) |
/// | [`with_decode_failure_route`](crate::SagaProps::with_decode_failure_route) | [`with_decode_failure_route`](Self::with_decode_failure_route) |
///
/// That is the migration path: the resident saga is the degenerate case of the
/// manager, not a separate mechanism.
pub struct SagaManagerProps<S: Saga, C = CodecUnset, Sub = SubscriptionUnset> {
    store: Arc<dyn EventStore>,
    producer: Arc<dyn Fn() -> S + Send + Sync>,
    codec: C,
    subscription: Sub,
    instance_idle_timeout: IdleTimeout,
    scheduler: Option<SchedulerProxy>,
    enqueue_policy: Option<Arc<dyn EnqueuePolicy>>,
    dead_letter_subscriber: Option<Arc<dyn DlqChildSpawn<SagaProcess<S>>>>,
    crash_restart_factory: Option<CrashRestartFactory>,
    decode_failure_route_fn: Option<DecodeFailureRouteFn>,
}

impl<S: Saga> SagaManagerProps<S, CodecUnset, SubscriptionUnset> {
    /// Begin a manager spawn.
    ///
    /// `store` is the EventStore every instance persists its own events to;
    /// each one uses the [`SagaId`](crate::SagaId) `Saga::correlate` named as
    /// its stream key.  `producer` constructs a saga instance and is called once
    /// per spawned instance, so anything the saga captures (e.g. an
    /// `AggregateProxy` to tell) must be re-acquirable.
    pub fn new(
        store: Arc<dyn EventStore>,
        producer: impl Fn() -> S + Send + Sync + 'static,
    ) -> Self {
        Self {
            store,
            producer: Arc::new(producer),
            codec: CodecUnset,
            subscription: SubscriptionUnset,
            instance_idle_timeout: IdleTimeout::Inherit,
            scheduler: None,
            enqueue_policy: None,
            dead_letter_subscriber: None,
            crash_restart_factory: None,
            decode_failure_route_fn: None,
        }
    }
}

impl<S: Saga, Sub> SagaManagerProps<S, CodecUnset, Sub> {
    /// Bind the codec used to encode and decode the instances' own events.
    pub fn with_codec(
        self,
        codec: Arc<dyn ErasedCodec<S::Event>>,
    ) -> SagaManagerProps<S, CodecSet<S::Event>, Sub> {
        SagaManagerProps {
            store: self.store,
            producer: self.producer,
            codec: CodecSet { codec },
            subscription: self.subscription,
            instance_idle_timeout: self.instance_idle_timeout,
            scheduler: self.scheduler,
            enqueue_policy: self.enqueue_policy,
            dead_letter_subscriber: self.dead_letter_subscriber,
            crash_restart_factory: self.crash_restart_factory,
            decode_failure_route_fn: self.decode_failure_route_fn,
        }
    }
}

impl<S: Saga, C> SagaManagerProps<S, C, SubscriptionUnset> {
    /// Subscribe the manager to an upstream `EventStore`.
    ///
    /// This is the only subscription in the whole fan-out: the manager runs one
    /// `DirectPollerProcess` and decodes each upstream record once, then routes
    /// the decoded event by [`Saga::correlate`].
    pub fn with_subscription(
        self,
        upstream_store: Arc<dyn EventStore>,
        upstream_codec: Arc<dyn ErasedCodec<S::SubscribedEvent>>,
        cursor: SequenceCursor,
    ) -> SagaManagerProps<S, C, SubscriptionSet<S>> {
        SagaManagerProps {
            store: self.store,
            producer: self.producer,
            codec: self.codec,
            subscription: SubscriptionSet {
                upstream_store,
                upstream_codec,
                cursor,
            },
            instance_idle_timeout: self.instance_idle_timeout,
            scheduler: self.scheduler,
            enqueue_policy: self.enqueue_policy,
            dead_letter_subscriber: self.dead_letter_subscriber,
            crash_restart_factory: self.crash_restart_factory,
            decode_failure_route_fn: self.decode_failure_route_fn,
        }
    }
}

impl<S: Saga, C, Sub> SagaManagerProps<S, C, Sub> {
    /// Passivate an instance that has been idle for `after`.
    ///
    /// The instance is stopped and leaves the manager's registry; a later event
    /// for the same correlation id spawns it again, restoring its state by
    /// replaying its own stream.  Without this call instances stay resident for
    /// the manager's lifetime.
    pub fn with_instance_passivation(mut self, after: Duration) -> Self {
        self.instance_idle_timeout = IdleTimeout::After(after);
        self
    }

    /// Inject the resident [`SchedulerProxy`](crate::SchedulerProxy) (from
    /// [`crate::spawn_scheduler`]) so every instance this manager spawns has its
    /// [`SagaEffect::schedule`](crate::SagaEffect::schedule) and
    /// [`SagaEffect::cancel_schedule`](crate::SagaEffect::cancel_schedule)
    /// effects drive real timers — the same wiring
    /// [`SagaProps::with_scheduler`](crate::SagaProps::with_scheduler) gives a
    /// standalone saga.
    ///
    /// Without it, schedule markers are still persisted on each instance's own
    /// stream but no timer fires until a scheduler-equipped incarnation replays
    /// them — which also means a manager whose `Saga::correlate` is constant
    /// (the degenerate single-instance case) does not reproduce
    /// `SagaProps::with_scheduler` unless this is called too.
    pub fn with_scheduler(mut self, scheduler: SchedulerProxy) -> Self {
        self.scheduler = Some(scheduler);
        self
    }

    /// Filter which saga failures reach the DLQ, for every instance this
    /// manager spawns — the same wiring
    /// [`SagaProps::with_enqueue_policy`](crate::SagaProps::with_enqueue_policy)
    /// gives a standalone saga.  Without it instances run the default policy,
    /// which enqueues every failure kind.
    ///
    /// The policy is shared, not per-instance: it is handed the
    /// [`SagaFailure`](crate::SagaFailure) alone, so an instance-specific
    /// decision belongs in the failure's own data rather than in separate
    /// policies.
    pub fn with_enqueue_policy(mut self, policy: Arc<dyn EnqueuePolicy>) -> Self {
        self.enqueue_policy = Some(policy);
        self
    }

    /// Register a dead-letter subscriber process for every instance this
    /// manager spawns — the same wiring
    /// [`SagaProps::with_dead_letter_subscriber`](crate::SagaProps::with_dead_letter_subscriber)
    /// gives a standalone saga.  Without it the dead letters instances write to
    /// their own streams have no subscriber to reach.
    ///
    /// Each instance starts its own DLQ direct poller over its own stream, so
    /// `subscriber` receives the dead letters of the whole fan-out and can tell
    /// them apart by [`DeadLetterEvent::saga_id`](crate::DeadLetterEvent).  A
    /// poller is a child of the instance it polls for, so passivating an
    /// instance releases its subscription and a later revival re-opens it,
    /// catching up on anything written meanwhile.
    pub fn with_dead_letter_subscriber<P>(mut self, subscriber: ProcessProxy<P>) -> Self
    where
        P: Process + Receive<DeadLetterEvent, Response = ()> + 'static,
    {
        self.dead_letter_subscriber = Some(make_dlq_child_spawn::<SagaProcess<S>, P>(subscriber));
        self
    }

    /// Register a factory for crash-restart re-dispatch on every instance this
    /// manager spawns — the same wiring
    /// [`SagaProps::with_crash_restart_factory`](crate::SagaProps::with_crash_restart_factory)
    /// gives a standalone saga.
    ///
    /// This is what makes passivation lossless for in-flight tells: an instance
    /// stopped mid-tell loses the in-memory intent, so when a later event
    /// revives it the pending tell can only be reconstructed from its persisted
    /// payload.  Without the factory replay records a synthetic
    /// [`SagaFailure::TellFailed`](crate::SagaFailure) for it instead of
    /// re-dispatching it.
    pub fn with_crash_restart_factory<F>(mut self, factory: F) -> Self
    where
        F: Fn(&[u8]) -> Option<TellIntent> + Send + Sync + 'static,
    {
        self.crash_restart_factory = Some(Arc::new(factory));
        self
    }

    /// Determine which spawned instance owns an upstream record the codec
    /// could not decode, so that instance's
    /// [`SagaFailure::DecodeFailed`](crate::SagaFailure::DecodeFailed) dead
    /// letter is recorded on its own stream.
    ///
    /// A decode failure carries no typed event, so `Saga::correlate` cannot
    /// name an owner for it the way it does for a decoded event.  Without this
    /// call the manager has nowhere to attribute a corrupt record and skips it
    /// — logged, the shared cursor still advances past it, same as an event
    /// `Saga::correlate` claims for no instance.  Returning `Some(saga_id)`
    /// routes the failure to that instance instead — spawning and replaying it
    /// first if it is not resident — through the same
    /// `SagaFailure::DecodeFailed` path a standalone
    /// [`SagaProps`](crate::SagaProps) records.  Returning `None` skips the
    /// record, same as leaving this unset.
    pub fn with_decode_failure_route<F>(mut self, route: F) -> Self
    where
        F: Fn(&AggregateId, u64) -> Option<SagaId> + Send + Sync + 'static,
    {
        self.decode_failure_route_fn = Some(Arc::new(route));
        self
    }
}

impl<S: Saga> SagaManagerProps<S, CodecSet<S::Event>, SubscriptionSet<S>> {
    /// Spawn the manager process.
    ///
    /// The upstream poller and every saga instance are runtime **children** of
    /// the manager, so stopping the manager cascade-stops the whole fan-out.
    pub async fn spawn(self, system: &ProcessSystem) -> SagaManagerProxy<S> {
        let store = self.store;
        // The proxy hands out dead letter queues over the very store the
        // manager writes to, so it keeps its own handle to it.
        let store_for_proxy = Arc::clone(&store);
        let producer = self.producer;
        let codec = self.codec.codec;
        let upstream = upstream_subscription::<S>(
            self.subscription.upstream_store,
            self.subscription.upstream_codec,
        );
        let cursor = self.subscription.cursor;
        let instance_idle_timeout = self.instance_idle_timeout;
        let scheduler = self.scheduler;
        let enqueue_policy = self.enqueue_policy.unwrap_or_else(default_enqueue_policy);
        let dead_letter_subscriber = self.dead_letter_subscriber;
        let crash_restart_factory = self.crash_restart_factory;
        let decode_failure_route_fn = self.decode_failure_route_fn;

        let props = Props::new(move || SagaManagerProcess::<S> {
            store: Arc::clone(&store),
            producer: Arc::clone(&producer),
            codec: Arc::clone(&codec),
            enqueue_policy: Arc::clone(&enqueue_policy),
            upstream: upstream.clone(),
            cursor: cursor.clone(),
            instance_idle_timeout,
            scheduler: scheduler.clone(),
            dlq_child_spawn: dead_letter_subscriber.clone(),
            crash_restart_factory: crash_restart_factory.clone(),
            decode_failure_route_fn: decode_failure_route_fn.clone(),
            instances: HashMap::new(),
        })
        // An instance that could not settle a record is reported by holding the
        // shared cursor, not by taking the manager down: stopping it would strip
        // every other instance of its delivery because one of them failed.
        .with_supervision_strategy(SupervisionStrategy::Resume)
        // Idleness is a per-instance signal — `with_instance_passivation` is
        // what acts on it.  The manager itself is the sole owner of the
        // subscription and the registry with nothing left to revive it, so
        // letting a system-wide default idle timeout reach it would silently
        // and permanently stop the whole fan-out during a quiet upstream.
        .with_idle_timeout(IdleTimeout::Persistent);

        SagaManagerProxy::new(system.spawn(props).await, store_for_proxy)
    }
}
