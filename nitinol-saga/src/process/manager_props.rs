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
/// The manager it spawns is a reader of the sources the saga observes, keeping
/// one offset per source and partition — see the crate's
/// [mental model](crate#mental-model).
///
/// It spawns a manager that reads each declared source — one reader per
/// partition, shared by every instance of the saga type that falls in it, each
/// resuming from an offset it records — and derives the
/// instance for each record from [`Saga::correlate`]: an id with no resident
/// instance is spawned and replayed on the spot, an id already resident is
/// handed the record directly. Beside it, it starts the delivery host that
/// carries out what the manager decides: a saga whose decisions nobody carries
/// out is not a saga, so the two are one declaration. A saga type whose
/// alphabet declares a deadline is also started with its deadline index, which
/// reads the schedule facts every instance has written and asks the manager
/// for an instance when one of its deadlines falls due — so a deadline reaches
/// its instance whether or not anything upstream ever names it again.
///
/// `B` is the binding typestate: one slot per declared source,
/// [`observing`](Self::observing) moves one, and [`spawn`](Self::spawn) exists
/// only once every slot is filled.  `H` is whether the system has been named
/// yet — [`new`](Self::new) builds an [`Unhosted`] one whose
/// [`spawn`](Self::spawn) takes a system, and
/// [`SagaSystemExt::host`](crate::SagaSystemExt::host) hands back a
/// [`SagaHosted`] one that carries it already. Neither names a store: the
/// journal its instances persist to, the journals its sources are observed
/// from, and the slot its delivery host and its source partitions record in
/// are all the system's.
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
    /// Nothing but the producer: the journal its instances persist their own
    /// events to, the journal its sources are read from and the slot its
    /// delivery host records in are all taken from the system
    /// [`spawn`](Self::spawn) is given.
    ///
    /// ```compile_fail
    /// # use nitinol_persistence::store::InMemoryEventStore;
    /// # use nitinol_saga::SagaManagerProps;
    /// # struct ReservationSaga;
    /// # fn wiring() {
    /// let store = InMemoryEventStore::default();
    /// // there is no form of this that takes a journal
    /// SagaManagerProps::<ReservationSaga>::new(store, |_| ReservationSaga);
    /// # }
    /// ```
    ///
    /// `producer` builds a saga instance from the capabilities the framework
    /// resolved for it and is called once per spawned instance, so anything the
    /// saga captures (a reference it reads the world through, say) must be
    /// re-acquirable.
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

    /// Spread this saga's delivery host over `partitions` independent
    /// processes.
    ///
    /// What a partition bounds, and what changing the count costs, is stated on
    /// [`SagaDeliveryProps::with_partitions`] and on [`SagaDeliveryProps`].
    ///
    /// The deadline index is spread over the same count and assigns a saga to
    /// the same partition, by [`partition_of`](crate::partition_of). It records
    /// no offset, so changing the count costs it nothing: every partition
    /// reads the schedule facts from the start whenever it starts.
    ///
    /// The manager reads every declared source with the same count of
    /// partitions, each assigned the instances [`partition_of`](crate::partition_of)
    /// puts in it, and each with an offset of its own. Changing the count is
    /// done on the delivery host's terms, for the same reason: an offset covers
    /// the instances that used to fall in its partition, so a new count leaves
    /// some partitions reading again what is already consumed — which the
    /// instances drop — and can leave a record unsettled under the old count
    /// behind an offset already past it under the new one. So every partition
    /// is drained first: stop the saga, let nothing be left unsettled, then
    /// start it with the new count. Changing the count under a running saga,
    /// or restarting with a new count while a partition is held at an
    /// unsettled record, can carry an offset past a record that then never
    /// reaches [`Saga::handle`] again.
    pub fn with_partitions(mut self, partitions: NonZeroUsize) -> Self {
        self.delivery = self.delivery.with_partitions(partitions);
        self
    }

    /// Have this saga's delivery host ask the journal no more often than every
    /// `interval` — see [`SagaDeliveryProps::with_poll_interval`].
    ///
    /// This is the *delivery* side's cadence, on the saga's own decisions, and
    /// the deadline index's: both read what the saga's own streams hold, so
    /// one setting says how stale either may be. A deadline is therefore
    /// asked for at most this long after it falls due. How
    /// often an observed source is asked is that source's own
    /// [`Subscription::with_poll_interval`], named per source on
    /// [`observing`](Self::observing).
    pub fn with_delivery_poll_interval(mut self, interval: Duration) -> Self {
        self.delivery = self.delivery.with_poll_interval(interval);
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
    /// These are the only readers in the fan-out: the manager reads each
    /// declared source with one partition per partition of the host
    /// ([`with_partitions`](Self::with_partitions), one by default), each of
    /// which decodes what it reads once and hands on the records whose
    /// instance, by [`Saga::correlate`], falls in it.  An instance the manager
    /// spawns opens no subscription of its own.
    ///
    /// Where a partition reads from is the higher of `subscription`'s floor and
    /// the offset the partition recorded in the system's saga-offset slot — the
    /// slot the delivery host records in, under a key of its own. The offset
    /// moves only as far as every record below it is settled, once per poll,
    /// so a restart reads on from there rather than sweeping the source again
    /// from its floor; a record whose decision never became durable stops the
    /// partition at it, reading nothing past it and handing it again on the
    /// next poll, and a restart brings it to [`Saga::handle`] again from the
    /// offset below it.  The
    /// floor is never undercut, whatever offset is recorded.  No instance's
    /// consumed position speaks for either: the consumed position is what drops a
    /// record an instance has already consumed, which is what keeps an offset
    /// that was lost or moved backwards from costing a second decision.
    ///
    /// An instance with a pending deadline and nothing new upstream is not
    /// brought back by reading the source: the deadline index reads the
    /// schedule fact on the instance's own stream and asks the manager for the
    /// instance when the deadline falls due.
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
    /// and — when the alphabet declares a deadline — the deadline index, on
    /// `system`.
    ///
    /// Every partition of every declared source and every saga instance are
    /// runtime **children** of the manager, so stopping the manager cascade-stops the
    /// whole fan-out.
    ///
    /// A saga whose alphabet declares a deadline also has the system's one
    /// resident timer core resolved here, and every instance the manager spawns
    /// is handed it, and its deadline index started beside the delivery host —
    /// which is the whole of a saga's deadline wiring, however many instances
    /// the correlation fans out to.
    ///
    /// Every instance the manager spawns is handed `system` through
    /// [`SagaComponents::system`](crate::SagaComponents::system).
    ///
    /// # Typestate
    ///
    /// Because a spawn always starts a host, this requires the store to hold the
    /// saga-offset slot that host records its progress in. A store that holds
    /// none is refused here, while the wiring is compiled, rather than when the
    /// host first tries to record where it got to:
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
    ///
    /// And a spawn names a system rather than a process system: the stores a
    /// saga needs come from a supplier, which a bare
    /// [`ProcessSystem`](nitinol_runtime::ProcessSystem) has none of.
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
    /// Spawn the manager process, the delivery host that carries out what it
    /// decides and — when the alphabet declares a deadline — the deadline
    /// index, on the system these props came from.
    ///
    /// Nothing is named here, and every instance the manager spawns is handed
    /// that system through
    /// [`SagaComponents::system`](crate::SagaComponents::system).  The manager
    /// starts first: a delivery host reads what is already journalled, so the
    /// order costs nothing either way, and the index asks that manager for
    /// instances. The handle answers for every part only once all of them are
    /// running.
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
    pub async fn spawn(self) -> SagaHostProxy<S> {
        let host = self.host.system.system().clone();
        self.start_on(&host).await
    }
}
