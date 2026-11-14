use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::Duration;

use nitinol_eventsource::system::{EventSourceSystem, Hosted, Unhosted};
use nitinol_eventsource::Event;
use nitinol_persistence::provider::SagaOffsetSlot;
use nitinol_runtime::process::SupervisionStrategy;
use nitinol_runtime::{IdleTimeout, Props};

use crate::components::SagaComponents;
use crate::deadline_index;
use crate::delivery::{SagaDeliveryId, SagaDeliveryProps};
use crate::process::instance::SagaProducer;
use crate::process::manager::SagaManagerProcess;
use crate::process::manager_offsets::SagaManagerOffsets;
use crate::process::proxy::{SagaHostProxy, SagaManagerProxy};
use crate::process::source_partition::UpstreamSource;
use crate::saga::Saga;
use crate::scheduler;
use crate::subscribe::{AllSourcesSubscribed, BindSource, SourceBindings};
use crate::subscription::Subscription;
use std::marker::PhantomData;

/// Where a hosted saga's props start: the system they came from.
///
/// `St` is that system's own store type, carried rather than erased so the
/// saga-offset slot a delivery host records its progress in is required at the
/// `spawn` of these props the same way it is required at the `spawn` of props
/// built directly.
pub struct SagaHosted<S: Saga, St> {
    system: Hosted<St>,
    _saga: PhantomData<fn() -> S>,
}

/// The builder every saga is started from.
///
/// [`spawn`](Self::spawn) starts a manager that reads each declared source and
/// hands every record to the instance [`Saga::correlate`] names, spawning and
/// replaying it if it is not resident; the delivery host that carries out the
/// saga's decisions; and, when the alphabet declares a deadline, the deadline
/// index that asks the manager for an instance whose deadline falls due. See
/// the crate's [mental model](crate#mental-model).
///
/// - [`observing`](Self::observing) binds one declared source, and `spawn`
///   exists only once every one is bound.
/// - Every store is the system's: [`new`](Self::new) props take the system at
///   `spawn`, [`SagaSystemExt::host`](crate::SagaSystemExt::host) props carry it.
/// - The source readers, the delivery host and the deadline index share one
///   partition count, set with [`with_partitions`](Self::with_partitions).
/// - A `correlate` that always returns the same id runs exactly one instance,
///   on the same terms as any other.
pub struct SagaManagerProps<S: Saga, B = SourceBindings<S>, H = Unhosted> {
    producer: SagaProducer<S>,
    bindings: B,
    instance_idle_timeout: IdleTimeout,
    /// The delivery host's settings, carried in every `H` because both entry
    /// points start a host and both name those settings on this builder.
    delivery: SagaDeliveryProps<S>,
    /// The host identifier the wiring named, if it named one.
    ///
    /// Kept here rather than only handed to `delivery`, because the manager's
    /// source offsets are filed under the same identifier and the spawn
    /// resolves it once for both.
    delivery_id: Option<SagaDeliveryId>,
    host: H,
}

impl<S: Saga> SagaManagerProps<S, SourceBindings<S>, Unhosted> {
    /// Begin a manager spawn, with every declared source still unbound.
    ///
    /// `producer` is called once per spawned instance, so anything the saga
    /// captures must be re-acquirable. Every store comes from the system
    /// [`spawn`](Self::spawn) is given.
    pub fn new(producer: impl Fn(SagaComponents<S>) -> S + Send + Sync + 'static) -> Self {
        Self {
            producer: Arc::new(producer),
            bindings: SourceBindings::<S>::default(),
            instance_idle_timeout: IdleTimeout::Inherit,
            delivery: SagaDeliveryProps::new(),
            delivery_id: None,
            host: Unhosted,
        }
    }
}

impl<S: Saga, B> SagaManagerProps<S, B, Unhosted> {
    /// Carry `host` as where these props start — as what the instances they
    /// spawn are handed through [`SagaComponents::system`], and as the system
    /// the delivery host starts its partitions on.
    pub(crate) fn hosted<St>(
        self,
        host: EventSourceSystem<St>,
    ) -> SagaManagerProps<S, B, SagaHosted<S, St>> {
        SagaManagerProps {
            producer: self.producer,
            bindings: self.bindings,
            instance_idle_timeout: self.instance_idle_timeout,
            delivery: self.delivery,
            delivery_id: self.delivery_id,
            host: SagaHosted {
                system: Hosted::new(host),
                _saga: PhantomData,
            },
        }
    }
}

impl<S: Saga, B, H> SagaManagerProps<S, B, H> {
    /// Name the delivery host this saga starts, so its recorded progress is
    /// filed under `id`.
    ///
    /// What the identifier is for, and what it defaults to, is stated on
    /// [`SagaDeliveryProps::with_delivery_id`].
    ///
    /// The manager's progress through each declared source is filed under the
    /// same identifier, so two hosts over one saga type keep their reading of
    /// the sources apart as well as their delivery.
    pub fn with_delivery_id(mut self, id: SagaDeliveryId) -> Self {
        self.delivery_id = Some(id);
        self
    }

    /// Spread this saga's delivery host, its source readers and its deadline
    /// index over `partitions` independent processes, each saga assigned by
    /// [`partition_of`](crate::partition_of).
    ///
    /// A source partition's offset covers the instances assigned to it under
    /// the count it ran with, so a new count is applied only after every
    /// partition is drained: stop the saga, let nothing be left unsettled, then
    /// start it with the new count. What a partition bounds and what changing
    /// the count costs are stated on [`SagaDeliveryProps`].
    pub fn with_partitions(mut self, partitions: NonZeroUsize) -> Self {
        self.delivery = self.delivery.with_partitions(partitions);
        self
    }

    /// Have this saga's delivery host and its deadline index ask the journal no
    /// more often than every `interval`; see
    /// [`SagaDeliveryProps::with_poll_interval`] and
    /// [`DEFAULT_POLL_INTERVAL`](nitinol_eventsource::DEFAULT_POLL_INTERVAL).
    ///
    /// A deadline is asked for at most this long after it falls due. How often
    /// an observed source is asked is set per source with
    /// [`Subscription::with_poll_interval`].
    pub fn with_delivery_poll_interval(mut self, interval: Duration) -> Self {
        self.delivery = self.delivery.with_poll_interval(interval);
        self
    }
}

impl<S: Saga, B, H> SagaManagerProps<S, B, H> {
    /// Observe the declared source `E` through the records `subscription`
    /// selects, decoded with the codec `E` names.
    ///
    /// Call once per source the alphabet declares: binding a source twice or
    /// one the alphabet never declares is a compile error. Each partition reads
    /// from the higher of `subscription`'s floor and the offset it recorded,
    /// and stops at a record whose decision has not become durable until it is
    /// settled. Which instance a record reaches is decided by
    /// [`Saga::correlate`].
    pub fn observing<E>(
        self,
        subscription: Subscription,
    ) -> SagaManagerProps<S, <B as BindSource<E>>::Bound, H>
    where
        E: Event,
        B: BindSource<E>,
    {
        SagaManagerProps {
            producer: self.producer,
            bindings: <B as BindSource<E>>::bind(self.bindings, subscription.into_binding()),
            instance_idle_timeout: self.instance_idle_timeout,
            delivery: self.delivery,
            delivery_id: self.delivery_id,
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
    /// The one road both entry points take: start the manager, the delivery
    /// host that carries out what it decides, and — when the alphabet declares
    /// a deadline — the deadline index, on `system`.
    ///
    /// Every store comes from `system` here and nowhere else — the journal its
    /// instances persist to and its sources are observed from, and the
    /// saga-offset slot the host and the source partitions record their
    /// progress in — so the two spawn forms cannot drift into reaching
    /// different destinations.
    ///
    /// The manager starts first: a delivery host reads what is already
    /// journalled, so the order costs nothing either way, and the index asks
    /// the manager it is handed for instances, so that manager exists before
    /// it. The handle answers for every part only once all of them are
    /// running.
    async fn start_on<St: SagaOffsetSlot>(
        self,
        system: &EventSourceSystem<St>,
    ) -> SagaHostProxy<S> {
        let store = system.persistence().event_store();
        let offsets = system.persistence().saga_offsets();
        let host = system.to_contingent();
        let ps = system.process_system();

        let producer = self.producer;
        let upstream: Vec<_> = self
            .bindings
            .into_sources()
            .into_iter()
            .map(UpstreamSource::resolve)
            .collect();
        let instance_idle_timeout = self.instance_idle_timeout;
        let scheduler = scheduler::resolve_for::<S>(ps).await;
        // Resolved once, here, and handed to both halves: the delivery host
        // files its progress under this identifier and the manager files its
        // sources' progress under it too, so the two cannot disagree on what
        // the host is called. The default is derived the way the delivery
        // host derives its own — from the declared event type, which the type
        // fixes in every build — because the key it names is persisted.
        let host_id = self.delivery_id.unwrap_or_else(|| {
            SagaDeliveryId::new(<S::Event as Event>::EVENT_TYPE.to_path().to_string())
        });
        let partitions = self.delivery.partitions();

        let manager_host = host.clone();
        let manager_store = store.clone();
        let manager_offsets = SagaManagerOffsets::new(offsets.clone());
        let manager_host_id = host_id.clone();
        let props = Props::new(move || SagaManagerProcess::<S> {
            store: manager_store.clone(),
            producer: Arc::clone(&producer),
            host: manager_host.clone(),
            upstream: upstream.clone(),
            offsets: manager_offsets.clone(),
            partitions,
            host_id: manager_host_id.clone(),
            instance_idle_timeout,
            scheduler: scheduler.clone(),
            instances: HashMap::new(),
        })
        // An instance that could not settle a record is reported by holding
        // its source partition, not by taking the manager down: stopping it
        // would strip every other instance of its delivery because one of them
        // failed.
        .with_supervision_strategy(SupervisionStrategy::Resume)
        // Idleness is a per-instance signal — `with_instance_passivation` is
        // what acts on it.  The manager itself is the sole owner of the
        // source partitions and the registry with nothing left to revive it,
        // so letting a system-wide default idle timeout reach it would
        // silently and permanently stop the whole fan-out during a quiet
        // upstream.
        .with_idle_timeout(IdleTimeout::Persistent);

        let manager = ps.spawn(props).await;
        let poll_interval = self.delivery.poll_interval();
        let delivery = self
            .delivery
            .with_delivery_id(host_id)
            .start_on(store.clone(), offsets, host)
            .await;
        let deadline_index =
            deadline_index::start_for::<S>(partitions, poll_interval, store, manager.clone(), ps)
                .await;

        SagaHostProxy::new(SagaManagerProxy::new(manager), delivery, deadline_index)
    }
}

impl<S: Saga, B> SagaManagerProps<S, B, Unhosted>
where
    B: AllSourcesSubscribed<S::Subscribe>,
{
    /// Spawn the manager, the delivery host that carries out what it decides
    /// and, when the alphabet declares a deadline, the deadline index, on
    /// `system`.
    ///
    /// Every source partition and every saga instance is a child of the
    /// manager, so stopping the manager stops all of them. Every instance is
    /// handed `system` through
    /// [`SagaComponents::system`](crate::SagaComponents::system), and one whose
    /// alphabet declares a deadline is handed the system's resident timer core
    /// with no further wiring.
    ///
    /// The store must hold the saga-offset slot the delivery host records its
    /// progress in; a store without one is a **compile error**:
    ///
    /// ```compile_fail
    /// # use async_trait::async_trait;
    /// # use bytes::Bytes;
    /// # use nitinol_eventsource::codec::Codec;
    /// # use nitinol_eventsource::system::EventSourceSystem;
    /// # use nitinol_eventsource::Event;
    /// # use nitinol_persistence::provider::{CustomStore, PersistenceProvider};
    /// # use nitinol_persistence::store::InMemoryEventStore;
    /// # use nitinol_persistence::{AggregateId, EventType, Family, TypeName};
    /// # use nitinol_runtime::ProcessSystem;
    /// # use nitinol_saga::{
    /// #     Saga, SagaContext, SagaEffect, SagaId, SagaManagerProps, Subscription,
    /// # };
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
    /// // no `saga_offsets` — this store has no saga slot
    /// let store = CustomStore::builder().eventstore(InMemoryEventStore::default()).build();
    /// let system = EventSourceSystem::builder(ps)
    ///     .with_persistence(PersistenceProvider::new(store))
    ///     .build();
    /// SagaManagerProps::<ReservationSaga>::new(|_| ReservationSaga)
    ///     .observing::<OrderPlaced>(Subscription::stream(&AggregateId::new("order")))
    ///     .spawn(&system)
    ///     .await;
    /// # }
    /// ```
    pub async fn spawn<St: SagaOffsetSlot>(
        self,
        system: &EventSourceSystem<St>,
    ) -> SagaHostProxy<S> {
        self.start_on(system).await
    }
}

impl<S, B, St> SagaManagerProps<S, B, SagaHosted<S, St>>
where
    S: Saga,
    B: AllSourcesSubscribed<S::Subscribe>,
    St: SagaOffsetSlot,
{
    /// Spawn the manager, the delivery host that carries out what it decides
    /// and, when the alphabet declares a deadline, the deadline index, on the
    /// system these props came from.
    ///
    /// Every instance the manager spawns is handed that system through
    /// [`SagaComponents::system`](crate::SagaComponents::system). The handle
    /// answers for every part only once all of them are running.
    ///
    /// Every declared source must be bound with [`observing`](Self::observing)
    /// before this exists: spawning with any source unbound is a
    /// **compile error**. Here the alphabet declares two sources and the
    /// builder is handed one:
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
    /// # use nitinol_saga::{Saga, SagaContext, SagaEffect, SagaId, SagaSystemExt, Subscription};
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
    ///     .observing::<OrderPlaced>(Subscription::stream(&order_id))
    ///     .spawn()
    ///     .await;
    /// # }
    /// ```
    pub async fn spawn(self) -> SagaHostProxy<S> {
        let host = self.host.system.system().clone();
        self.start_on(&host).await
    }
}

#[cfg(doctest)]
mod compile_fail_fixtures {
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
    fn spawn_requires_every_declared_source_bound() {}

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
    /// # use nitinol_saga::{Saga, SagaContext, SagaEffect, SagaId, SagaSystemExt, Subscription};
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
    ///     .observing::<OrderPlaced>(Subscription::stream(&order_id))
    ///     .observing::<OrderPlaced>(Subscription::stream(&order_id))
    ///     .spawn()
    ///     .await;
    /// # }
    /// ```
    fn binding_one_source_twice_is_refused() {}

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
    /// # use nitinol_saga::{Saga, SagaContext, SagaEffect, SagaId, SagaSystemExt, Subscription};
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
    ///     .observing::<Undeclared>(Subscription::stream(&order_id))
    ///     .spawn()
    ///     .await;
    /// # }
    /// ```
    fn binding_an_undeclared_source_is_refused() {}
}
