#[cfg(test)]
use std::collections::HashMap;
use std::sync::Arc;

use nitinol_eventsource::system::{Hosted, SystemHandle, Unhosted};
use nitinol_eventsource::Event;
use nitinol_persistence::store::EventStore;
use nitinol_runtime::ProcessSystem;

use crate::components::SagaComponents;
use crate::dead_letter::{self, DeadLetterEvent, DlqChildSpawn, EnqueuePolicy};
use crate::effect::TellIntent;
use crate::id::SagaId;
use crate::process::instance::{SagaInstanceSpec, SagaProducer, ScheduleDelivery};
use crate::process::proxy::SagaProxy;
#[cfg(test)]
use crate::process::saga_process::TellState;
use crate::process::saga_process::{self, CrashRestartFactory, SagaProcess};
use crate::saga::Saga;
use crate::scheduler;
use crate::subscribe::{AllSourcesSubscribed, BindSource, SourceBinding, SourceBindings};
use crate::subscription::Subscription;
use nitinol_runtime::process::{Process, ProcessProxy, Receive};

/// Builder for a saga spawn.
///
/// # The binding typestate
///
/// `B` carries one slot per source the saga's alphabet declares, and
/// [`subscribed_to`](Self::subscribed_to) moves exactly the slot of the event
/// type it is given.  [`spawn`](Self::spawn) exists only on the shape where every
/// slot is filled, so a declared source left unbound is a compile error rather
/// than a saga that silently never hears from it.  A `#[subscribe(deadline)]`
/// variant is allotted no slot: a deadline is addressed to the instance that set
/// it rather than observed, so there is nothing to subscribe to.
///
/// Binding the same source twice, and binding a source the alphabet never
/// declared, are compile errors for the same reason: neither has a slot the move
/// could apply to.
///
/// There is no codec stage: every type in `S::Subscribe` and `S::Event` names the
/// codec that writes and reads it, so a saga has no format of its own to be
/// handed and no way to journal itself in one its event type never named.
///
/// There is no scheduler stage either.  Whether this saga has its own time is
/// settled by whether its alphabet declares a deadline, so the host resolves the
/// resident timer core from the declaration — see [`SagaComponents::deadline`].
pub struct SagaProps<S: Saga, B = SourceBindings<S>, H = Unhosted> {
    saga_id: SagaId,
    store: Arc<dyn EventStore>,
    producer: SagaProducer<S>,
    bindings: B,
    crash_restart_factory: Option<CrashRestartFactory>,
    enqueue_policy: Option<Arc<dyn EnqueuePolicy>>,
    dead_letter_subscriber: Option<Arc<dyn DlqChildSpawn<SagaProcess<S>>>>,
    host: H,
    #[cfg(test)]
    initial_tell_states: HashMap<u64, TellState>,
}

impl<S: Saga> SagaProps<S, SourceBindings<S>, Unhosted> {
    /// Begin a saga spawn, with every declared source still unbound.
    ///
    /// `producer` builds the saga instance from the capabilities the framework
    /// resolved for it, and is called again whenever the runtime rebuilds the
    /// process — so anything the saga captures (e.g. an `AggregateProxy` to tell)
    /// must be re-acquirable.  A saga that needs no capability is built by
    /// `|_| S::default()`.
    pub fn new(
        saga_id: SagaId,
        store: Arc<dyn EventStore>,
        producer: impl Fn(SagaComponents<S>) -> S + Send + Sync + 'static,
    ) -> Self {
        Self {
            saga_id,
            store,
            producer: Arc::new(producer),
            bindings: SourceBindings::<S>::default(),
            crash_restart_factory: None,
            enqueue_policy: None,
            dead_letter_subscriber: None,
            host: Unhosted,
            #[cfg(test)]
            initial_tell_states: HashMap::new(),
        }
    }
}

impl<S: Saga, B> SagaProps<S, B, Unhosted> {
    /// Carry `host` as where these props start, and as what their incarnations
    /// are handed through [`SagaComponents::system`].
    pub(crate) fn hosted(self, host: SystemHandle) -> SagaProps<S, B, Hosted> {
        SagaProps {
            saga_id: self.saga_id,
            store: self.store,
            producer: self.producer,
            bindings: self.bindings,
            crash_restart_factory: self.crash_restart_factory,
            enqueue_policy: self.enqueue_policy,
            dead_letter_subscriber: self.dead_letter_subscriber,
            host: Hosted::new(host),
            #[cfg(test)]
            initial_tell_states: self.initial_tell_states,
        }
    }
}

impl<S: Saga, B, H> SagaProps<S, B, H> {
    /// Observe the declared source `E` through `subscription`.
    ///
    /// At spawn time each bound source starts a runtime child
    /// `DirectPollerProcess` of its own (catchup + live, at-least-once), with its
    /// own cursor.  The pollers are cascade-stopped when the saga stops.
    ///
    /// Sources are independent: there is no order across them, and a source
    /// whose upstream has stalled does not stop another from progressing.  Two
    /// sources may read one stream key — each takes only the rows naming its own
    /// event type.
    ///
    /// `subscription`'s position is where this source's poller starts *at the
    /// latest*: it is carried forward to the position the saga's own stream
    /// records for this source, so a restart does not re-read what it has already
    /// consumed.  The configured position still holds as a floor.
    ///
    /// Which of the delivered events belong to this instance is decided by
    /// [`Saga::correlate`], not here — correlation is domain knowledge carried
    /// by the saga type.
    pub fn subscribed_to<E>(
        self,
        subscription: Subscription,
    ) -> SagaProps<S, <B as BindSource<E>>::Bound, H>
    where
        E: Event,
        B: BindSource<E>,
    {
        let (upstream_store, cursor) = subscription.into_parts();
        SagaProps {
            saga_id: self.saga_id,
            store: self.store,
            producer: self.producer,
            bindings: <B as BindSource<E>>::bind(
                self.bindings,
                SourceBinding::new(upstream_store, cursor),
            ),
            crash_restart_factory: self.crash_restart_factory,
            enqueue_policy: self.enqueue_policy,
            dead_letter_subscriber: self.dead_letter_subscriber,
            host: self.host,
            #[cfg(test)]
            initial_tell_states: self.initial_tell_states,
        }
    }

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

    #[cfg(test)]
    pub(crate) fn with_initial_tell_states(mut self, tell_states: HashMap<u64, TellState>) -> Self {
        self.initial_tell_states = tell_states;
        self
    }
}

impl<S: Saga, B, H> SagaProps<S, B, H>
where
    B: AllSourcesSubscribed<S::Subscribe>,
{
    /// The resolved ingredients of this saga's instance, on `system`.
    ///
    /// The sole place a configured props becomes a spec, so both spawn entry
    /// points start the same instance.
    async fn into_spec(
        self,
        system: &ProcessSystem,
        host: Option<SystemHandle>,
    ) -> SagaInstanceSpec<S> {
        let scheduler = scheduler::resolve_for::<S>(system).await;
        SagaInstanceSpec::<S> {
            saga_id: self.saga_id,
            store: self.store,
            producer: self.producer,
            host,
            enqueue_policy: self
                .enqueue_policy
                .unwrap_or_else(dead_letter::default_enqueue_policy),
            upstream: self
                .bindings
                .into_sources()
                .into_iter()
                .map(saga_process::source_subscription)
                .collect(),
            crash_restart_factory: self.crash_restart_factory,
            scheduler,
            // Nothing fronts this saga, so a deadline's firing is addressed to
            // the process itself; there is no manager to restore it from its
            // own stream.
            delivery: ScheduleDelivery::ToTheInstance,
            dlq_child_spawn: self.dead_letter_subscriber,
            #[cfg(test)]
            initial_tell_states: self.initial_tell_states,
        }
    }
}

impl<S: Saga, B> SagaProps<S, B, Unhosted>
where
    B: AllSourcesSubscribed<S::Subscribe>,
{
    /// Spawn the saga process on `system`.
    ///
    /// Each declared source is wired as a direct poller child process of its own
    /// — no shared [`nitinol_eventsource::DurableStream`] fan-out channel is
    /// created.  Their lifetimes are bound to the saga process; when the saga
    /// stops the runtime cascade-stops every poller.
    ///
    /// A saga whose alphabet declares a deadline also has the system's one
    /// resident timer core resolved for it here, which is the whole of its
    /// scheduler wiring.
    ///
    /// These props were built on a store rather than taken from an
    /// `EventSourceSystem`, so there is no host to hand over:
    /// [`SagaComponents::system`](crate::SagaComponents::system) answers
    /// `None` for every incarnation this starts.
    pub async fn spawn(self, system: &ProcessSystem) -> SagaProxy<S> {
        let spec = self.into_spec(system, None).await;
        system.spawn(spec.into_props()).await.into()
    }
}

impl<S: Saga, B> SagaProps<S, B, Hosted>
where
    B: AllSourcesSubscribed<S::Subscribe>,
{
    /// Spawn the saga process on the system these props came from.
    ///
    /// Nothing is named here, and every incarnation is handed that system
    /// through [`SagaComponents::system`](crate::SagaComponents::system).
    pub async fn spawn(self) -> SagaProxy<S> {
        let host = self.host.system().clone();
        let ps = host.process_system().clone();
        let spec = self.into_spec(&ps, Some(host)).await;
        ps.spawn(spec.into_props()).await.into()
    }
}
