use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use nitinol_eventsource::system::{Hosted, SystemHandle, Unhosted};
use nitinol_eventsource::Event;
use nitinol_persistence::store::EventStore;
use nitinol_runtime::process::SupervisionStrategy;
use nitinol_runtime::{IdleTimeout, ProcessSystem, Props};

use crate::components::SagaComponents;
use crate::process::instance::SagaProducer;
use crate::process::manager::SagaManagerProcess;
use crate::process::proxy::SagaManagerProxy;
use crate::process::saga_process;
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
/// same replay, same effect interpretation, same append ordering.  There is no
/// instance-level setting to carry over: what an instance does is settled by
/// the saga type itself, and the one knob this builder adds —
/// [`with_instance_passivation`](Self::with_instance_passivation) — is about
/// residency rather than behaviour.  A saga's own time needs no wiring either:
/// the host resolves the resident timer core from the saga's alphabet, so a
/// manager's instances get it on exactly the same terms a standalone saga does.
///
/// That is the migration path: the resident saga is the degenerate case of the
/// manager, not a separate mechanism.
pub struct SagaManagerProps<S: Saga, B = SourceBindings<S>, H = Unhosted> {
    store: Arc<dyn EventStore>,
    producer: SagaProducer<S>,
    bindings: B,
    instance_idle_timeout: IdleTimeout,
    host: H,
}

impl<S: Saga> SagaManagerProps<S, SourceBindings<S>, Unhosted> {
    /// Begin a manager spawn, with every declared source still unbound.
    ///
    /// `store` is the EventStore every instance persists its own events to;
    /// each one uses the [`SagaId`](crate::SagaId) `Saga::correlate` named as
    /// its stream key.  `producer` builds a saga instance from the capabilities
    /// the framework resolved for it and is called once per spawned instance, so
    /// anything the saga captures (a reference it reads the world through, say)
    /// must be re-acquirable.
    pub fn new(
        store: Arc<dyn EventStore>,
        producer: impl Fn(SagaComponents<S>) -> S + Send + Sync + 'static,
    ) -> Self {
        Self {
            store,
            producer: Arc::new(producer),
            bindings: SourceBindings::<S>::default(),
            instance_idle_timeout: IdleTimeout::Inherit,
            host: Unhosted,
        }
    }
}

impl<S: Saga, B> SagaManagerProps<S, B, Unhosted> {
    /// Carry `host` as where these props start, and as what the instances they
    /// spawn are handed through [`SagaComponents::system`].
    pub(crate) fn hosted(self, host: SystemHandle) -> SagaManagerProps<S, B, Hosted> {
        SagaManagerProps {
            store: self.store,
            producer: self.producer,
            bindings: self.bindings,
            instance_idle_timeout: self.instance_idle_timeout,
            host: Hosted::new(host),
        }
    }
}

impl<S: Saga, B, H> SagaManagerProps<S, B, H> {
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
    ) -> SagaManagerProps<S, <B as BindSource<E>>::Bound, H>
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
            host: self.host,
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
}

impl<S: Saga, B, H> SagaManagerProps<S, B, H>
where
    B: AllSourcesSubscribed<S::Subscribe>,
{
    /// Start the manager on `system`, handing `host` to every instance it
    /// spawns.
    ///
    /// The sole place a configured props becomes a running manager, so both
    /// spawn entry points start the same fan-out.
    async fn start_on(
        self,
        system: &ProcessSystem,
        host: Option<SystemHandle>,
    ) -> SagaManagerProxy<S> {
        let store = self.store;
        let producer = self.producer;
        let upstream: Vec<_> = self
            .bindings
            .into_sources()
            .into_iter()
            .map(saga_process::source_subscription)
            .collect();
        let instance_idle_timeout = self.instance_idle_timeout;
        let scheduler = scheduler::resolve_for::<S>(system).await;

        let props = Props::new(move || SagaManagerProcess::<S> {
            store: Arc::clone(&store),
            producer: Arc::clone(&producer),
            host: host.clone(),
            upstream: upstream.clone(),
            instance_idle_timeout,
            scheduler: scheduler.clone(),
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

        SagaManagerProxy::new(system.spawn(props).await)
    }
}

impl<S: Saga, B> SagaManagerProps<S, B, Unhosted>
where
    B: AllSourcesSubscribed<S::Subscribe>,
{
    /// Spawn the manager process on `system`.
    ///
    /// Every declared source's poller and every saga instance are runtime
    /// **children** of the manager, so stopping the manager cascade-stops the
    /// whole fan-out.
    ///
    /// A saga whose alphabet declares a deadline also has the system's one
    /// resident timer core resolved here, and every instance the manager spawns
    /// is handed it — so the degenerate single-instance manager reproduces a
    /// standalone saga's deadlines with no extra wiring.
    ///
    /// These props were built on a store rather than taken from an
    /// `EventSourceSystem`, so there is no host to hand over:
    /// [`SagaComponents::system`](crate::SagaComponents::system) answers
    /// `None` for every instance this manager spawns.
    pub async fn spawn(self, system: &ProcessSystem) -> SagaManagerProxy<S> {
        self.start_on(system, None).await
    }
}

impl<S: Saga, B> SagaManagerProps<S, B, Hosted>
where
    B: AllSourcesSubscribed<S::Subscribe>,
{
    /// Spawn the manager process on the system these props came from.
    ///
    /// Nothing is named here, and every instance the manager spawns is handed
    /// that system through
    /// [`SagaComponents::system`](crate::SagaComponents::system).
    pub async fn spawn(self) -> SagaManagerProxy<S> {
        let host = self.host.system().clone();
        let ps = host.process_system().clone();
        self.start_on(&ps, Some(host)).await
    }
}
