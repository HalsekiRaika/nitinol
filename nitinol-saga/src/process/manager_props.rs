use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::Duration;

use nitinol_eventsource::system::{Hosted, SystemHandle, Unhosted};
use nitinol_eventsource::Event;
use nitinol_persistence::store::{Journal, OffsetStore};
use nitinol_runtime::process::SupervisionStrategy;
use nitinol_runtime::{IdleTimeout, ProcessSystem, Props};

use crate::components::SagaComponents;
use crate::delivery::{SagaDeliveryId, SagaDeliveryProps};
use crate::process::instance::SagaProducer;
use crate::process::manager::SagaManagerProcess;
use crate::process::proxy::{SagaHostProxy, SagaManagerProxy};
use crate::process::saga_process;
use crate::saga::Saga;
use crate::scheduler;
use crate::subscribe::{AllSourcesSubscribed, BindSource, SourceBindings};
use crate::subscription::Subscription;

/// Where a hosted saga's props start: the system they came from, and the
/// delivery host wiring resolved from the same supplier.
///
/// A saga whose decisions nobody carries out is not a saga, so the two are one
/// state rather than two wirings — which is why the delivery host's settings
/// are named on this builder and there is no second declaration to forget.
/// `Cs` is the offset store the supplier's saga slot is configured with, which
/// the partitions name statically.
pub struct SagaHosted<S: Saga, Cs: OffsetStore> {
    system: Hosted,
    delivery: SagaDeliveryProps<S, Cs, Unhosted>,
}

/// The builder every saga is started from.
///
/// It spawns a manager that holds one upstream subscription per declared
/// source — shared by every instance of the saga type — and derives the
/// instance for each record from [`Saga::correlate`]: an id with no resident
/// instance is spawned and replayed on the spot, an id already resident is
/// handed the record directly.
///
/// `B` is the binding typestate: one slot per declared source,
/// [`observing`](Self::observing) moves one, and [`spawn`](Self::spawn) exists
/// only once every slot is filled.  `H` is where the props start —
/// [`SagaSystemExt::host`](crate::SagaSystemExt::host) hands back a
/// [`SagaHosted`] one, journalling onto the store the system's provider
/// supplies and carrying the delivery host wiring resolved from it, and
/// [`new`](Self::new) builds an [`Unhosted`] one on a store named outright.
///
/// # A single resident saga
///
/// A [`Saga::correlate`] that always answers the same id reduces the manager to
/// exactly one instance — same replay, same effect interpretation, same append
/// ordering as any other instance.  That is not a second mechanism to reach for
/// but this one's degenerate case, which is why there is no separate builder
/// for it: what an instance does is settled by the saga type itself, and the
/// one knob this builder adds —
/// [`with_instance_passivation`](Self::with_instance_passivation) — is about
/// residency rather than behaviour.  A saga's own time needs no wiring either:
/// the host resolves the resident timer core from the saga's alphabet, so one
/// instance gets its deadlines on exactly the same terms a thousand do.
pub struct SagaManagerProps<S: Saga, B = SourceBindings<S>, H = Unhosted> {
    store: Journal,
    producer: SagaProducer<S>,
    bindings: B,
    instance_idle_timeout: IdleTimeout,
    host: H,
}

impl<S: Saga> SagaManagerProps<S, SourceBindings<S>, Unhosted> {
    /// Begin a manager spawn journalling onto `store`, with every declared
    /// source still unbound.
    ///
    /// This is the explicit form for a deployment whose instances journal into
    /// a destination of their own; one journalling onto the store its system's
    /// provider supplies says so with
    /// [`SagaSystemExt::host`](crate::SagaSystemExt::host) and names no store
    /// at all.
    ///
    /// `store` is the EventStore every instance persists its own events to;
    /// each one uses the [`SagaId`](crate::SagaId) `Saga::correlate` named as
    /// its stream key.  `producer` builds a saga instance from the capabilities
    /// the framework resolved for it and is called once per spawned instance, so
    /// anything the saga captures (a reference it reads the world through, say)
    /// must be re-acquirable.
    ///
    /// The journal arrives as whatever the deployment holds: a backend handle,
    /// a [`Journal`] a supplier answered with, or a reference to one it keeps.
    pub fn new(
        store: impl Into<Journal>,
        producer: impl Fn(SagaComponents<S>) -> S + Send + Sync + 'static,
    ) -> Self {
        Self {
            store: store.into(),
            producer: Arc::new(producer),
            bindings: SourceBindings::<S>::default(),
            instance_idle_timeout: IdleTimeout::Inherit,
            host: Unhosted,
        }
    }
}

impl<S: Saga, B> SagaManagerProps<S, B, Unhosted> {
    /// Carry `host` as where these props start — as what the instances they
    /// spawn are handed through [`SagaComponents::system`], and as the system
    /// `delivery` starts its partitions on.
    pub(crate) fn hosted<Cs>(
        self,
        host: SystemHandle,
        delivery: SagaDeliveryProps<S, Cs, Unhosted>,
    ) -> SagaManagerProps<S, B, SagaHosted<S, Cs>>
    where
        Cs: OffsetStore,
    {
        SagaManagerProps {
            store: self.store,
            producer: self.producer,
            bindings: self.bindings,
            instance_idle_timeout: self.instance_idle_timeout,
            host: SagaHosted {
                system: Hosted::new(host),
                delivery,
            },
        }
    }
}

impl<S, B, Cs> SagaManagerProps<S, B, SagaHosted<S, Cs>>
where
    S: Saga,
    Cs: OffsetStore + Clone + 'static,
{
    /// Name the delivery host this saga starts, so its recorded progress is
    /// filed under `id`.
    ///
    /// What the identifier is for, and what it defaults to, is stated on
    /// [`SagaDeliveryProps::with_delivery_id`].
    pub fn with_delivery_id(mut self, id: SagaDeliveryId) -> Self {
        self.host.delivery = self.host.delivery.with_delivery_id(id);
        self
    }

    /// Spread this saga's delivery host over `partitions` independent
    /// processes.
    ///
    /// What a partition bounds, and what changing the count costs, is stated on
    /// [`SagaDeliveryProps::with_partitions`] and on [`SagaDeliveryProps`].
    pub fn with_partitions(mut self, partitions: NonZeroUsize) -> Self {
        self.host.delivery = self.host.delivery.with_partitions(partitions);
        self
    }

    /// Have this saga's delivery host ask the journal no more often than every
    /// `interval` — see [`SagaDeliveryProps::with_poll_interval`].
    ///
    /// This is the *delivery* side's cadence, on the saga's own decisions. How
    /// often an observed source is asked is that source's own
    /// [`Subscription::with_poll_interval`], named per source on
    /// [`observing`](Self::observing).
    pub fn with_delivery_poll_interval(mut self, interval: Duration) -> Self {
        self.host.delivery = self.host.delivery.with_poll_interval(interval);
        self
    }
}

impl<S: Saga, B, H> SagaManagerProps<S, B, H> {
    /// Observe the declared source `E` through the records `subscription`
    /// selects, decoded with the codec `E` names.
    ///
    /// One call per source the saga's alphabet declares, and one call only: the
    /// slot this moves has moved, so binding a source twice and binding a
    /// source the alphabet never declared are both compile errors, and
    /// [`spawn`](Self::spawn) does not exist while any slot is still unbound.
    ///
    /// These are the only subscriptions in the fan-out: the manager runs one
    /// `DirectPollerProcess` per declared source and decodes each record once,
    /// then routes it by [`Saga::correlate`].  An instance the manager spawns
    /// opens none of its own — which is also why a source's cursor starts
    /// exactly where `subscription` says: it is spoken for by no single
    /// instance's consumed position, and the sweep from there is what activates
    /// the instances that are not resident.  Each instance still drops the
    /// records its own stream names as consumed, so the sweep costs no second
    /// decision.
    ///
    /// Which of the delivered events an instance acts on is decided by
    /// [`Saga::correlate`], not here — correlation is domain knowledge carried
    /// by the saga type.
    pub fn observing<E>(
        self,
        subscription: Subscription,
    ) -> SagaManagerProps<S, <B as BindSource<E>>::Bound, H>
    where
        E: Event,
        B: BindSource<E>,
    {
        SagaManagerProps {
            store: self.store,
            producer: self.producer,
            bindings: <B as BindSource<E>>::bind(self.bindings, subscription.into_binding()),
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
            store: store.clone(),
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
    /// is handed it — which is the whole of a saga's deadline wiring, however
    /// many instances the correlation fans out to.
    ///
    /// These props were built on a store rather than taken from an
    /// `EventSourceSystem`, so there is no host to hand over:
    /// [`SagaComponents::system`](crate::SagaComponents::system) answers
    /// `None` for every instance this manager spawns.
    pub async fn spawn(self, system: &ProcessSystem) -> SagaManagerProxy<S> {
        self.start_on(system, None).await
    }
}

impl<S, B, Cs> SagaManagerProps<S, B, SagaHosted<S, Cs>>
where
    S: Saga,
    B: AllSourcesSubscribed<S::Subscribe>,
    Cs: OffsetStore + Clone + 'static,
{
    /// Spawn the manager process, and the delivery host that carries out what
    /// it decides, on the system these props came from.
    ///
    /// Nothing is named here, and every instance the manager spawns is handed
    /// that system through
    /// [`SagaComponents::system`](crate::SagaComponents::system).  The manager
    /// starts first: a delivery host reads what is already journalled, so the
    /// order costs nothing either way, and the handle answers for both halves
    /// only once both are running.
    ///
    /// Spawning before every declared source is bound is a **compile error** —
    /// including the degenerate case of binding nothing at all:
    ///
    /// ```compile_fail
    /// # use async_trait::async_trait;
    /// # use bytes::Bytes;
    /// # use nitinol_eventsource::codec::Codec;
    /// # use nitinol_eventsource::system::EventSourceSystem;
    /// # use nitinol_eventsource::Event;
    /// # use nitinol_persistence::provider::PersistenceProvider;
    /// # use nitinol_persistence::store::InMemoryStore;
    /// # use nitinol_persistence::{EventType, Family, TypeName};
    /// # use nitinol_runtime::ProcessSystem;
    /// # use nitinol_saga::{Saga, SagaContext, SagaEffect, SagaId, SagaSystemExt};
    /// # struct DocCodec;
    /// # #[derive(Clone)] struct OrderPlaced;
    /// # #[derive(Clone)] struct ReservationRequested;
    /// # impl Codec<OrderPlaced> for DocCodec {
    /// #     type Error = std::convert::Infallible;
    /// #     fn encode(_: &OrderPlaced) -> Result<Bytes, Self::Error> { Ok(Bytes::new()) }
    /// #     fn decode(_: &[u8]) -> Result<OrderPlaced, Self::Error> { Ok(OrderPlaced) }
    /// # }
    /// # impl Codec<ReservationRequested> for DocCodec {
    /// #     type Error = std::convert::Infallible;
    /// #     fn encode(_: &ReservationRequested) -> Result<Bytes, Self::Error> { Ok(Bytes::new()) }
    /// #     fn decode(_: &[u8]) -> Result<ReservationRequested, Self::Error> { Ok(ReservationRequested) }
    /// # }
    /// # impl Event for OrderPlaced {
    /// #     const EVENT_TYPE: EventType = EventType::new(Family::new("doc"), TypeName::new("OrderPlaced"));
    /// #     type Codec = DocCodec;
    /// # }
    /// # impl Event for ReservationRequested {
    /// #     const EVENT_TYPE: EventType = EventType::new(Family::new("doc"), TypeName::new("ReservationRequested"));
    /// #     type Codec = DocCodec;
    /// # }
    /// # use nitinol::saga::SagaSubscribe;
    /// # #[derive(SagaSubscribe)] enum Observed { Order(OrderPlaced) }
    /// # struct ReservationSaga;
    /// # #[async_trait]
    /// # impl Saga for ReservationSaga {
    /// #     type Subscribe = Observed;
    /// #     type Event = ReservationRequested;
    /// #     type Error = std::convert::Infallible;
    /// #     fn correlate(_: &Observed) -> Option<SagaId> { Some(SagaId::new("reservation")) }
    /// #     async fn handle(&mut self, _: Observed, _: &mut SagaContext)
    /// #         -> Result<SagaEffect<ReservationRequested>, Self::Error> { Ok(SagaEffect::None) }
    /// # }
    /// # async fn wiring() {
    /// # let ps = ProcessSystem::new().await;
    /// # let system = EventSourceSystem::builder(ps)
    /// #     .with_persistence(PersistenceProvider::new(InMemoryStore::default()))
    /// #     .build();
    /// system
    ///     .host::<ReservationSaga>(|_| ReservationSaga)
    ///     .spawn()
    ///     .await;
    /// # }
    /// ```
    ///
    /// So is binding only *some* of the declared sources.  The alphabet below
    /// declares two, and the builder is handed one:
    ///
    /// ```compile_fail
    /// # use async_trait::async_trait;
    /// # use bytes::Bytes;
    /// # use nitinol_eventsource::codec::Codec;
    /// # use nitinol_eventsource::system::EventSourceSystem;
    /// # use nitinol_eventsource::Event;
    /// # use nitinol_persistence::provider::PersistenceProvider;
    /// # use nitinol_persistence::store::InMemoryStore;
    /// # use nitinol_persistence::{AggregateId, EventType, Family, TypeName};
    /// # use nitinol_runtime::ProcessSystem;
    /// # use nitinol_saga::{Saga, SagaContext, SagaEffect, SagaId, SagaSubscriptionExt, SagaSystemExt};
    /// # struct DocCodec;
    /// # #[derive(Clone)] struct OrderPlaced;
    /// # #[derive(Clone)] struct StockReserved;
    /// # #[derive(Clone)] struct ReservationRequested;
    /// # impl Codec<OrderPlaced> for DocCodec {
    /// #     type Error = std::convert::Infallible;
    /// #     fn encode(_: &OrderPlaced) -> Result<Bytes, Self::Error> { Ok(Bytes::new()) }
    /// #     fn decode(_: &[u8]) -> Result<OrderPlaced, Self::Error> { Ok(OrderPlaced) }
    /// # }
    /// # impl Codec<StockReserved> for DocCodec {
    /// #     type Error = std::convert::Infallible;
    /// #     fn encode(_: &StockReserved) -> Result<Bytes, Self::Error> { Ok(Bytes::new()) }
    /// #     fn decode(_: &[u8]) -> Result<StockReserved, Self::Error> { Ok(StockReserved) }
    /// # }
    /// # impl Codec<ReservationRequested> for DocCodec {
    /// #     type Error = std::convert::Infallible;
    /// #     fn encode(_: &ReservationRequested) -> Result<Bytes, Self::Error> { Ok(Bytes::new()) }
    /// #     fn decode(_: &[u8]) -> Result<ReservationRequested, Self::Error> { Ok(ReservationRequested) }
    /// # }
    /// # impl Event for OrderPlaced {
    /// #     const EVENT_TYPE: EventType = EventType::new(Family::new("doc"), TypeName::new("OrderPlaced"));
    /// #     type Codec = DocCodec;
    /// # }
    /// # impl Event for StockReserved {
    /// #     const EVENT_TYPE: EventType = EventType::new(Family::new("doc"), TypeName::new("StockReserved"));
    /// #     type Codec = DocCodec;
    /// # }
    /// # impl Event for ReservationRequested {
    /// #     const EVENT_TYPE: EventType = EventType::new(Family::new("doc"), TypeName::new("ReservationRequested"));
    /// #     type Codec = DocCodec;
    /// # }
    /// # use nitinol::saga::SagaSubscribe;
    /// # #[derive(SagaSubscribe)] enum Observed { Order(OrderPlaced), Stock(StockReserved) }
    /// # struct ReservationSaga;
    /// # #[async_trait]
    /// # impl Saga for ReservationSaga {
    /// #     type Subscribe = Observed;
    /// #     type Event = ReservationRequested;
    /// #     type Error = std::convert::Infallible;
    /// #     fn correlate(_: &Observed) -> Option<SagaId> { Some(SagaId::new("reservation")) }
    /// #     async fn handle(&mut self, _: Observed, _: &mut SagaContext)
    /// #         -> Result<SagaEffect<ReservationRequested>, Self::Error> { Ok(SagaEffect::None) }
    /// # }
    /// # async fn wiring() {
    /// # let ps = ProcessSystem::new().await;
    /// # let system = EventSourceSystem::builder(ps)
    /// #     .with_persistence(PersistenceProvider::new(InMemoryStore::default()))
    /// #     .build();
    /// # let order_id = AggregateId::new("order");
    /// // `StockReserved` is declared and never bound, so `spawn` does not exist
    /// system
    ///     .host::<ReservationSaga>(|_| ReservationSaga)
    ///     .observing::<OrderPlaced>(system.subscription(&order_id))
    ///     .spawn()
    ///     .await;
    /// # }
    /// ```
    ///
    /// Binding one source twice is a compile error too — the slot has already
    /// moved, so there is no transition left to apply:
    ///
    /// ```compile_fail
    /// # use async_trait::async_trait;
    /// # use bytes::Bytes;
    /// # use nitinol_eventsource::codec::Codec;
    /// # use nitinol_eventsource::system::EventSourceSystem;
    /// # use nitinol_eventsource::Event;
    /// # use nitinol_persistence::provider::PersistenceProvider;
    /// # use nitinol_persistence::store::InMemoryStore;
    /// # use nitinol_persistence::{AggregateId, EventType, Family, TypeName};
    /// # use nitinol_runtime::ProcessSystem;
    /// # use nitinol_saga::{Saga, SagaContext, SagaEffect, SagaId, SagaSubscriptionExt, SagaSystemExt};
    /// # struct DocCodec;
    /// # #[derive(Clone)] struct OrderPlaced;
    /// # #[derive(Clone)] struct ReservationRequested;
    /// # impl Codec<OrderPlaced> for DocCodec {
    /// #     type Error = std::convert::Infallible;
    /// #     fn encode(_: &OrderPlaced) -> Result<Bytes, Self::Error> { Ok(Bytes::new()) }
    /// #     fn decode(_: &[u8]) -> Result<OrderPlaced, Self::Error> { Ok(OrderPlaced) }
    /// # }
    /// # impl Codec<ReservationRequested> for DocCodec {
    /// #     type Error = std::convert::Infallible;
    /// #     fn encode(_: &ReservationRequested) -> Result<Bytes, Self::Error> { Ok(Bytes::new()) }
    /// #     fn decode(_: &[u8]) -> Result<ReservationRequested, Self::Error> { Ok(ReservationRequested) }
    /// # }
    /// # impl Event for OrderPlaced {
    /// #     const EVENT_TYPE: EventType = EventType::new(Family::new("doc"), TypeName::new("OrderPlaced"));
    /// #     type Codec = DocCodec;
    /// # }
    /// # impl Event for ReservationRequested {
    /// #     const EVENT_TYPE: EventType = EventType::new(Family::new("doc"), TypeName::new("ReservationRequested"));
    /// #     type Codec = DocCodec;
    /// # }
    /// # use nitinol::saga::SagaSubscribe;
    /// # #[derive(SagaSubscribe)] enum Observed { Order(OrderPlaced) }
    /// # struct ReservationSaga;
    /// # #[async_trait]
    /// # impl Saga for ReservationSaga {
    /// #     type Subscribe = Observed;
    /// #     type Event = ReservationRequested;
    /// #     type Error = std::convert::Infallible;
    /// #     fn correlate(_: &Observed) -> Option<SagaId> { Some(SagaId::new("reservation")) }
    /// #     async fn handle(&mut self, _: Observed, _: &mut SagaContext)
    /// #         -> Result<SagaEffect<ReservationRequested>, Self::Error> { Ok(SagaEffect::None) }
    /// # }
    /// # async fn wiring() {
    /// # let ps = ProcessSystem::new().await;
    /// # let system = EventSourceSystem::builder(ps)
    /// #     .with_persistence(PersistenceProvider::new(InMemoryStore::default()))
    /// #     .build();
    /// # let order_id = AggregateId::new("order");
    /// system
    ///     .host::<ReservationSaga>(|_| ReservationSaga)
    ///     .observing::<OrderPlaced>(system.subscription(&order_id))
    ///     .observing::<OrderPlaced>(system.subscription(&order_id))
    ///     .spawn()
    ///     .await;
    /// # }
    /// ```
    ///
    /// And so is binding a source the alphabet never declared:
    ///
    /// ```compile_fail
    /// # use async_trait::async_trait;
    /// # use bytes::Bytes;
    /// # use nitinol_eventsource::codec::Codec;
    /// # use nitinol_eventsource::system::EventSourceSystem;
    /// # use nitinol_eventsource::Event;
    /// # use nitinol_persistence::provider::PersistenceProvider;
    /// # use nitinol_persistence::store::InMemoryStore;
    /// # use nitinol_persistence::{AggregateId, EventType, Family, TypeName};
    /// # use nitinol_runtime::ProcessSystem;
    /// # use nitinol_saga::{Saga, SagaContext, SagaEffect, SagaId, SagaSubscriptionExt, SagaSystemExt};
    /// # struct DocCodec;
    /// # #[derive(Clone)] struct OrderPlaced;
    /// # #[derive(Clone)] struct Undeclared;
    /// # #[derive(Clone)] struct ReservationRequested;
    /// # impl Codec<OrderPlaced> for DocCodec {
    /// #     type Error = std::convert::Infallible;
    /// #     fn encode(_: &OrderPlaced) -> Result<Bytes, Self::Error> { Ok(Bytes::new()) }
    /// #     fn decode(_: &[u8]) -> Result<OrderPlaced, Self::Error> { Ok(OrderPlaced) }
    /// # }
    /// # impl Codec<Undeclared> for DocCodec {
    /// #     type Error = std::convert::Infallible;
    /// #     fn encode(_: &Undeclared) -> Result<Bytes, Self::Error> { Ok(Bytes::new()) }
    /// #     fn decode(_: &[u8]) -> Result<Undeclared, Self::Error> { Ok(Undeclared) }
    /// # }
    /// # impl Codec<ReservationRequested> for DocCodec {
    /// #     type Error = std::convert::Infallible;
    /// #     fn encode(_: &ReservationRequested) -> Result<Bytes, Self::Error> { Ok(Bytes::new()) }
    /// #     fn decode(_: &[u8]) -> Result<ReservationRequested, Self::Error> { Ok(ReservationRequested) }
    /// # }
    /// # impl Event for OrderPlaced {
    /// #     const EVENT_TYPE: EventType = EventType::new(Family::new("doc"), TypeName::new("OrderPlaced"));
    /// #     type Codec = DocCodec;
    /// # }
    /// # impl Event for Undeclared {
    /// #     const EVENT_TYPE: EventType = EventType::new(Family::new("doc"), TypeName::new("Undeclared"));
    /// #     type Codec = DocCodec;
    /// # }
    /// # impl Event for ReservationRequested {
    /// #     const EVENT_TYPE: EventType = EventType::new(Family::new("doc"), TypeName::new("ReservationRequested"));
    /// #     type Codec = DocCodec;
    /// # }
    /// # use nitinol::saga::SagaSubscribe;
    /// # #[derive(SagaSubscribe)] enum Observed { Order(OrderPlaced) }
    /// # struct ReservationSaga;
    /// # #[async_trait]
    /// # impl Saga for ReservationSaga {
    /// #     type Subscribe = Observed;
    /// #     type Event = ReservationRequested;
    /// #     type Error = std::convert::Infallible;
    /// #     fn correlate(_: &Observed) -> Option<SagaId> { Some(SagaId::new("reservation")) }
    /// #     async fn handle(&mut self, _: Observed, _: &mut SagaContext)
    /// #         -> Result<SagaEffect<ReservationRequested>, Self::Error> { Ok(SagaEffect::None) }
    /// # }
    /// # async fn wiring() {
    /// # let ps = ProcessSystem::new().await;
    /// # let system = EventSourceSystem::builder(ps)
    /// #     .with_persistence(PersistenceProvider::new(InMemoryStore::default()))
    /// #     .build();
    /// # let order_id = AggregateId::new("order");
    /// system
    ///     .host::<ReservationSaga>(|_| ReservationSaga)
    ///     .observing::<Undeclared>(system.subscription(&order_id))
    ///     .spawn()
    ///     .await;
    /// # }
    /// ```
    pub async fn spawn(self) -> SagaHostProxy<S> {
        let SagaManagerProps {
            store,
            producer,
            bindings,
            instance_idle_timeout,
            host: SagaHosted { system, delivery },
        } = self;

        let host = system.system().clone();
        let ps = host.process_system().clone();

        let manager = SagaManagerProps {
            store,
            producer,
            bindings,
            instance_idle_timeout,
            host: Unhosted,
        }
        .start_on(&ps, Some(host.clone()))
        .await;

        SagaHostProxy::new(manager, delivery.start_on(host).await)
    }
}
