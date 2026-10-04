use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use nitinol_eventsource::Event;
use nitinol_persistence::store::EventStore;
use nitinol_runtime::process::{Process, ProcessProxy, Receive, SupervisionStrategy};
use nitinol_runtime::{IdleTimeout, ProcessSystem, Props};

use crate::components::SagaComponents;
use crate::dead_letter::{self, DeadLetterEvent, DlqChildSpawn, EnqueuePolicy};
use crate::effect::TellIntent;
use crate::process::instance::SagaProducer;
use crate::process::manager::SagaManagerProcess;
use crate::process::proxy::SagaManagerProxy;
use crate::process::saga_process::{self, CrashRestartFactory, SagaProcess};
use crate::saga::Saga;
use crate::scheduler;
use crate::subscribe::{AllSourcesSubscribed, BindSource, SourceBinding, SourceBindings};
use crate::subscription::Subscription;

/// Builder for the saga instance manager.
///
/// Where [`SagaProps`](crate::SagaProps) spawns one saga bound to one
/// [`SagaId`](crate::SagaId), this spawns a manager that holds one upstream
/// subscription per declared source — shared by every instance of the saga
/// type — and derives the instance for each record from [`Saga::correlate`]:
/// an id with no resident instance is spawned and replayed on the spot, an id
/// already resident is handed the record directly.
///
/// The binding typestate mirrors [`SagaProps`](crate::SagaProps): `B` carries one
/// slot per declared source, [`subscribed_to`](Self::subscribed_to) moves one,
/// and [`spawn`](Self::spawn) exists only once every slot is filled.
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
/// | [`with_enqueue_policy`](crate::SagaProps::with_enqueue_policy) | [`with_enqueue_policy`](Self::with_enqueue_policy) |
/// | [`with_dead_letter_subscriber`](crate::SagaProps::with_dead_letter_subscriber) | [`with_dead_letter_subscriber`](Self::with_dead_letter_subscriber) |
/// | [`with_crash_restart_factory`](crate::SagaProps::with_crash_restart_factory) | [`with_crash_restart_factory`](Self::with_crash_restart_factory) |
///
/// A saga's own time needs no entry in that table: the host resolves the resident
/// timer core from the saga's alphabet, so a manager's instances get it on
/// exactly the same terms a standalone saga does.
///
/// That is the migration path: the resident saga is the degenerate case of the
/// manager, not a separate mechanism.
pub struct SagaManagerProps<S: Saga, B = SourceBindings<S>> {
    store: Arc<dyn EventStore>,
    producer: SagaProducer<S>,
    bindings: B,
    instance_idle_timeout: IdleTimeout,
    enqueue_policy: Option<Arc<dyn EnqueuePolicy>>,
    dead_letter_subscriber: Option<Arc<dyn DlqChildSpawn<SagaProcess<S>>>>,
    crash_restart_factory: Option<CrashRestartFactory>,
}

impl<S: Saga> SagaManagerProps<S, SourceBindings<S>> {
    /// Begin a manager spawn, with every declared source still unbound.
    ///
    /// `store` is the EventStore every instance persists its own events to;
    /// each one uses the [`SagaId`](crate::SagaId) `Saga::correlate` named as
    /// its stream key.  `producer` builds a saga instance from the capabilities
    /// the framework resolved for it and is called once per spawned instance, so
    /// anything the saga captures (e.g. an `AggregateProxy` to tell) must be
    /// re-acquirable.
    pub fn new(
        store: Arc<dyn EventStore>,
        producer: impl Fn(SagaComponents<S>) -> S + Send + Sync + 'static,
    ) -> Self {
        Self {
            store,
            producer: Arc::new(producer),
            bindings: SourceBindings::<S>::default(),
            instance_idle_timeout: IdleTimeout::Inherit,
            enqueue_policy: None,
            dead_letter_subscriber: None,
            crash_restart_factory: None,
        }
    }
}

impl<S: Saga, B> SagaManagerProps<S, B> {
    /// Observe the declared source `E` through `subscription`, for the whole
    /// fan-out.
    ///
    /// These are the only subscriptions in the fan-out: the manager runs one
    /// `DirectPollerProcess` per declared source and decodes each record once,
    /// then routes it by [`Saga::correlate`].  An instance the manager spawns
    /// opens none of its own.
    pub fn subscribed_to<E>(
        self,
        subscription: Subscription,
    ) -> SagaManagerProps<S, <B as BindSource<E>>::Bound>
    where
        E: Event,
        B: BindSource<E>,
    {
        let (upstream_store, cursor) = subscription.into_parts();
        SagaManagerProps {
            store: self.store,
            producer: self.producer,
            bindings: <B as BindSource<E>>::bind(
                self.bindings,
                SourceBinding::new(upstream_store, cursor),
            ),
            instance_idle_timeout: self.instance_idle_timeout,
            enqueue_policy: self.enqueue_policy,
            dead_letter_subscriber: self.dead_letter_subscriber,
            crash_restart_factory: self.crash_restart_factory,
        }
    }

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
        self.dead_letter_subscriber = Some(dead_letter::make_dlq_child_spawn::<SagaProcess<S>, P>(
            subscriber,
        ));
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
}

impl<S: Saga, B> SagaManagerProps<S, B>
where
    B: AllSourcesSubscribed<S::Subscribe>,
{
    /// Spawn the manager process.
    ///
    /// Every declared source's poller and every saga instance are runtime
    /// **children** of the manager, so stopping the manager cascade-stops the
    /// whole fan-out.
    ///
    /// A saga whose alphabet declares a deadline also has the system's one
    /// resident timer core resolved here, and every instance the manager spawns
    /// is handed it — so the degenerate single-instance manager reproduces a
    /// standalone saga's deadlines with no extra wiring.
    pub async fn spawn(self, system: &ProcessSystem) -> SagaManagerProxy<S> {
        let store = self.store;
        // The proxy hands out dead letter queues over the very store the
        // manager writes to, so it keeps its own handle to it.
        let store_for_proxy = Arc::clone(&store);
        let producer = self.producer;
        let upstream: Vec<_> = self
            .bindings
            .into_sources()
            .into_iter()
            .map(saga_process::source_subscription)
            .collect();
        let instance_idle_timeout = self.instance_idle_timeout;
        let scheduler = scheduler::resolve_for::<S>(system).await;
        let enqueue_policy = self
            .enqueue_policy
            .unwrap_or_else(dead_letter::default_enqueue_policy);
        let dead_letter_subscriber = self.dead_letter_subscriber;
        let crash_restart_factory = self.crash_restart_factory;

        let props = Props::new(move || SagaManagerProcess::<S> {
            store: Arc::clone(&store),
            producer: Arc::clone(&producer),
            enqueue_policy: Arc::clone(&enqueue_policy),
            upstream: upstream.clone(),
            instance_idle_timeout,
            scheduler: scheduler.clone(),
            dlq_child_spawn: dead_letter_subscriber.clone(),
            crash_restart_factory: crash_restart_factory.clone(),
            instances: HashMap::new(),
        })
        // An instance that could not settle a record is reported by holding the
        // shared cursor, not by taking the manager down: stopping it would strip
        // every other instance of its delivery because one of them failed.
        .with_supervision_strategy(SupervisionStrategy::Resume)
        // Idleness is a per-instance signal — `with_instance_passivation` is
        // what acts on it.  The manager itself is the sole owner of the
        // subscriptions and the registry with nothing left to revive it, so
        // letting a system-wide default idle timeout reach it would silently
        // and permanently stop the whole fan-out during a quiet upstream.
        .with_idle_timeout(IdleTimeout::Persistent);

        SagaManagerProxy::new(system.spawn(props).await, store_for_proxy)
    }
}
