//! Spawning a saga, and wiring a delivery host, from an [`EventSourceSystem`].
//!
//! # Why an extension trait
//!
//! `nitinol-saga` depends on `nitinol-eventsource`, so an inherent
//! `spawn_saga` on [`EventSourceSystem`] would have to live in
//! `nitinol-eventsource` and make that crate depend back on this one.  Declaring
//! the traits here keeps the dependency arrow pointing `saga -> eventsource`:
//! they are local, so implementing them for the foreign [`EventSourceSystem`]
//! is allowed, and the public entry points they need —
//! [`EventSourceSystem::process_system`] and
//! [`EventSourceSystem::persistence`] — already exist.

use std::borrow::Borrow;

use nitinol_eventsource::system::{EventSourceSystem, Hosted};
use nitinol_eventsource::Event;
use nitinol_persistence::provider::{DeliveryCheckpointSlot, Store};
use nitinol_persistence::store::CheckpointStore;

use crate::components::SagaComponents;
use crate::delivery::SagaDeliveryProps;
use crate::id::SagaId;
use crate::process::{SagaManagerProps, SagaProps, SagaProxy};
use crate::saga::Saga;
use crate::subscribe::{AllSourcesSubscribed, BindSource, SourceBindings};
use crate::subscription::Subscription;

/// Spawning a saga, a saga manager, or an upstream subscription from an
/// [`EventSourceSystem`].
///
/// This is the saga counterpart of [`EventSourceSystem::spawn_aggregate`]: the
/// caller names the saga, and the journal comes from the provider the system was
/// built with. The formats are settled already — every type in `S::Subscribe`
/// and `S::Event` names the codec that writes and reads it.
///
/// A saga touches two kinds of store: the one it journals its own events onto,
/// and the ones it polls for the sources it observes. Both are the provider's
/// — [`spawn_saga`](Self::spawn_saga) for the journal,
/// [`subscription`](Self::subscription) for a source — because
/// [`EventStore`] is stream-keyed and one instance holds the saga's own stream
/// next to the aggregate streams it reacts to.
///
/// Observing a source in a *different* destination is [`Subscription::stream`],
/// which names a store; journalling into one is [`SagaProps::new`] on the store
/// a second provider supplies. Both are explicit, which is what reaching two
/// destinations has to be.
///
/// [`SagaProps`] remains the way to spawn a saga that needs the settings this
/// entry point does not take (a dead-letter subscriber, an enqueue policy, or a
/// crash-restart factory).
pub trait SagaSystemExt {
    /// Begin a saga spawn journalling onto the store the provider supplies.
    ///
    /// `producer` builds the saga instance from the capabilities the framework
    /// resolved for it, and is called again if the runtime restarts the process,
    /// so anything the saga captures (e.g. an `AggregateProxy` to tell) must be
    /// re-acquirable.  A saga that needs no capability is built by
    /// `|_| S::default()`.
    ///
    /// The returned builder needs every source its alphabet declares bound
    /// before it can be spawned:
    ///
    /// ```rust,no_run
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
    /// let saga_proxy = system
    ///     .spawn_saga(SagaId::new("reservation"), |_| ReservationSaga)
    ///     .subscribed_to::<OrderPlaced>(system.subscription(&order_id))
    ///     .spawn()
    ///     .await;
    /// # }
    /// ```
    fn spawn_saga<S: Saga>(
        &self,
        saga_id: SagaId,
        producer: impl Fn(SagaComponents<S>) -> S + Send + Sync + 'static,
    ) -> SagaSpawn<S>;

    /// Subscribe to the stream `key` in the store the provider supplies, from
    /// its beginning.
    ///
    /// The counterpart of [`Subscription::stream`], which names a store
    /// explicitly and is how a source in another destination is observed.  The
    /// returned value is an ordinary [`Subscription`], so
    /// [`with_after`](Subscription::with_after) resumes from a position already
    /// processed.
    fn subscription<K>(&self, key: &K) -> Subscription
    where
        K: ?Sized + Borrow<str>;

    /// Return a [`SagaManagerProps`] journalling onto the store the provider
    /// supplies.
    ///
    /// One store holds the observed streams and every correlated instance's own
    /// stream side by side, keyed by the [`SagaId`] [`Saga::correlate`] names.
    /// Which sources the manager observes stays the caller's choice, through
    /// [`SagaManagerProps::subscribed_to`].  A manager whose instances must
    /// journal into another destination is built from [`SagaManagerProps::new`]
    /// instead.
    fn saga_manager_props<S>(
        &self,
        producer: impl Fn(SagaComponents<S>) -> S + Send + Sync + 'static,
    ) -> SagaManagerProps<S, SourceBindings<S>, Hosted>
    where
        S: Saga;
}

impl<St: Store> SagaSystemExt for EventSourceSystem<St> {
    fn spawn_saga<S: Saga>(
        &self,
        saga_id: SagaId,
        producer: impl Fn(SagaComponents<S>) -> S + Send + Sync + 'static,
    ) -> SagaSpawn<S> {
        SagaSpawn {
            props: SagaProps::new(saga_id, self.persistence().event_store(), producer)
                .hosted(self.erase_store()),
        }
    }

    fn subscription<K>(&self, key: &K) -> Subscription
    where
        K: ?Sized + Borrow<str>,
    {
        Subscription::stream(&self.persistence().event_store(), key)
    }

    fn saga_manager_props<S>(
        &self,
        producer: impl Fn(SagaComponents<S>) -> S + Send + Sync + 'static,
    ) -> SagaManagerProps<S, SourceBindings<S>, Hosted>
    where
        S: Saga,
    {
        SagaManagerProps::<S>::new(self.persistence().event_store(), producer)
            .hosted(self.erase_store())
    }
}

/// Wiring a delivery host from an [`EventSourceSystem`].
///
/// A host needs two slots — the journal the saga's decisions are on, and the
/// delivery-checkpoint slot its progress is recorded in — and this resolves
/// both from the provider the system was built with.
///
/// It is a trait of its own rather than part of [`SagaSystemExt`] because the
/// delivery slot is one a provider need not supply: a deployment that wires no
/// delivery host owes no store for one, and one that does is told so while the
/// wiring is compiled.
pub trait SagaDeliveryExt {
    /// The backend's own checkpoint store, as the provider's delivery slot is
    /// configured with it.
    type DeliveryCheckpoints: CheckpointStore + 'static;

    /// Begin wiring a delivery host over the saga type `S`.
    ///
    /// # Typestate
    ///
    /// This exists only where the provider supplies delivery checkpoints.
    /// A provider that supplies none is refused here rather than when the host
    /// first tries to record where it got to:
    ///
    /// ```compile_fail
    /// # use async_trait::async_trait;
    /// # use bytes::Bytes;
    /// # use nitinol_eventsource::codec::Codec;
    /// # use nitinol_eventsource::system::EventSourceSystem;
    /// # use nitinol_eventsource::Event;
    /// # use nitinol_persistence::provider::{CustomStore, PersistenceProvider};
    /// # use nitinol_persistence::store::InMemoryEventStore;
    /// # use nitinol_persistence::{EventType, Family, TypeName};
    /// # use nitinol_runtime::ProcessSystem;
    /// # use nitinol_saga::{Saga, SagaContext, SagaDeliveryExt, SagaEffect, SagaId};
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
    /// // no `delivery_checkpoints` — this store has no delivery slot
    /// let store = CustomStore::builder().eventstore(InMemoryEventStore::default()).build();
    /// let system = EventSourceSystem::builder(ps)
    ///     .with_persistence(PersistenceProvider::new(store))
    ///     .build();
    /// system.saga_delivery_props::<ReservationSaga>();
    /// # }
    /// ```
    fn saga_delivery_props<S: Saga>(
        &self,
    ) -> SagaDeliveryProps<S, Self::DeliveryCheckpoints, Hosted>;
}

impl<St: DeliveryCheckpointSlot> SagaDeliveryExt for EventSourceSystem<St> {
    type DeliveryCheckpoints = St::DeliveryCheckpoints;

    fn saga_delivery_props<S: Saga>(
        &self,
    ) -> SagaDeliveryProps<S, Self::DeliveryCheckpoints, Hosted> {
        SagaDeliveryProps::new(self.persistence()).hosted(self.erase_store())
    }
}

/// Builder returned by [`SagaSystemExt::spawn_saga`].
///
/// `B` carries the binding typestate — one slot per source the saga's alphabet
/// declares — so [`spawn`](Self::spawn) exists only once
/// [`subscribed_to`](Self::subscribed_to) has filled every one of them.  The
/// requirement is enforced by the compiler rather than at spawn time.
///
/// The journal store is already settled by the entry point that produced this
/// builder, so what is left to carry is the `ProcessSystem` to spawn into.
pub struct SagaSpawn<S: Saga, B = SourceBindings<S>> {
    props: SagaProps<S, B, Hosted>,
}

impl<S: Saga, B> SagaSpawn<S, B> {
    /// Observe the declared source `E` through the records `subscription`
    /// selects, decoded with the codec `E` names.
    ///
    /// Which of the delivered events this instance acts on is decided by
    /// [`Saga::correlate`], not here — and, of those, the ones it has not
    /// already consumed, which the framework decides from the positions on the
    /// saga's own stream ([`Saga::handle`]).
    pub fn subscribed_to<E>(
        self,
        subscription: Subscription,
    ) -> SagaSpawn<S, <B as BindSource<E>>::Bound>
    where
        E: Event,
        B: BindSource<E>,
    {
        SagaSpawn {
            props: self.props.subscribed_to::<E>(subscription),
        }
    }
}

impl<S: Saga, B> SagaSpawn<S, B>
where
    B: AllSourcesSubscribed<S::Subscribe>,
{
    /// Spawn the saga into the system's `ProcessSystem`.
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
    ///     .spawn_saga(SagaId::new("reservation"), |_| ReservationSaga)
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
    /// # use nitinol_saga::{Saga, SagaContext, SagaEffect, SagaId, SagaSystemExt};
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
    ///     .spawn_saga(SagaId::new("reservation"), |_| ReservationSaga)
    ///     .subscribed_to::<OrderPlaced>(system.subscription(&order_id))
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
    /// # let order_id = AggregateId::new("order");
    /// system
    ///     .spawn_saga(SagaId::new("reservation"), |_| ReservationSaga)
    ///     .subscribed_to::<OrderPlaced>(system.subscription(&order_id))
    ///     .subscribed_to::<OrderPlaced>(system.subscription(&order_id))
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
    /// # use nitinol_saga::{Saga, SagaContext, SagaEffect, SagaId, SagaSystemExt};
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
    ///     .spawn_saga(SagaId::new("reservation"), |_| ReservationSaga)
    ///     .subscribed_to::<Undeclared>(system.subscription(&order_id))
    ///     .spawn()
    ///     .await;
    /// # }
    /// ```
    pub async fn spawn(self) -> SagaProxy<S> {
        self.props.spawn().await
    }
}
