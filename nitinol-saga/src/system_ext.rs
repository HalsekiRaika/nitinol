//! Spawning a saga from an [`EventSourceSystem`], symmetrically to
//! [`EventSourceSystem::spawn_aggregate`].
//!
//! # Why an extension trait
//!
//! `nitinol-saga` depends on `nitinol-eventsource`, so an inherent
//! `spawn_saga` on [`EventSourceSystem`] would have to live in
//! `nitinol-eventsource` and make that crate depend back on this one.  Declaring
//! the trait here keeps the dependency arrow pointing `saga -> eventsource`:
//! the trait is local, so implementing it for the foreign
//! [`EventSourceSystem`] is allowed, and the public entry point it needs —
//! [`EventSourceSystem::process_system`] — already exists.

use std::borrow::Borrow;
use std::sync::Arc;

use nitinol_eventsource::system::{EventSourceSystem, StoreSet, StoreUnset};
use nitinol_eventsource::Event;
use nitinol_persistence::store::EventStore;
use nitinol_runtime::ProcessSystem;

use crate::components::SagaComponents;
use crate::id::SagaId;
use crate::process::{SagaManagerProps, SagaProps, SagaProxy};
use crate::saga::Saga;
use crate::subscribe::{AllSourcesSubscribed, BindSource, SourceBindings};
use crate::subscription::Subscription;

/// Spawns a [`Saga`] from an [`EventSourceSystem`].
///
/// This is the saga counterpart of
/// [`EventSourceSystem::spawn_aggregate`]: the caller names the saga and its
/// own [`EventStore`], and the system supplies the `ProcessSystem` to spawn
/// into.  The formats are settled already — every type in `S::Subscribe` and
/// `S::Event` names the codec that writes and reads it.
///
/// [`SagaProps`] remains the way to spawn a saga that needs the settings this
/// entry point does not take (a dead-letter subscriber, an enqueue policy, or a
/// crash-restart factory).
pub trait SagaSystemExt {
    /// Begin a saga spawn against this system.
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
    /// # use std::sync::Arc;
    /// # use async_trait::async_trait;
    /// # use bytes::Bytes;
    /// # use nitinol_eventsource::codec::Codec;
    /// # use nitinol_eventsource::system::EventSourceSystem;
    /// # use nitinol_eventsource::Event;
    /// # use nitinol_persistence::store::{EventStore, InMemoryEventStore};
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
    /// # let system = EventSourceSystem::builder(ps).build();
    /// # let order_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    /// # let order_id = AggregateId::new("order");
    /// # let saga_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    /// let saga_proxy = system
    ///     .spawn_saga(SagaId::new("reservation"), saga_store, |_| ReservationSaga)
    ///     .subscribed_to::<OrderPlaced>(Subscription::stream(&order_store, &order_id))
    ///     .spawn()
    ///     .await;
    /// # }
    /// ```
    fn spawn_saga<S: Saga>(
        &self,
        saga_id: SagaId,
        store: Arc<dyn EventStore>,
        producer: impl Fn(SagaComponents<S>) -> S + Send + Sync + 'static,
    ) -> SagaSpawn<'_, S>;
}

impl SagaSystemExt for EventSourceSystem<StoreUnset> {
    fn spawn_saga<S: Saga>(
        &self,
        saga_id: SagaId,
        store: Arc<dyn EventStore>,
        producer: impl Fn(SagaComponents<S>) -> S + Send + Sync + 'static,
    ) -> SagaSpawn<'_, S> {
        SagaSpawn::new(self.process_system(), saga_id, store, producer)
    }
}

/// Spawning a saga, a saga manager, or an upstream subscription from an
/// [`EventSourceSystem`] that carries a default [`EventStore`].
///
/// A saga touches two kinds of store: the one it journals its own events onto,
/// and the ones it polls for the sources it observes.  Both are the system's
/// default here — [`spawn_saga`](Self::spawn_saga) for the journal,
/// [`subscription`](Self::subscription) for a source — because [`EventStore`] is
/// stream-keyed and one instance holds the saga's own stream next to the
/// aggregate streams it reacts to.
///
/// Each direction keeps its own override, which takes precedence over the
/// default: [`spawn_saga_with_store`](Self::spawn_saga_with_store) names a
/// journal, [`Subscription::stream`] names a source's store.
///
/// This trait exists alongside [`SagaSystemExt`] rather than replacing it: a
/// system built without [`with_event_store`][nitinol_eventsource::system::EventSourceSystemBuilder::with_event_store]
/// has no default to resolve, so it keeps the store-per-spawn entry point and
/// none of the store-less ones.
pub trait SagaDefaultStoreExt {
    /// Begin a saga spawn that journals onto the system's default store.
    ///
    /// `producer` builds the saga instance from the capabilities the framework
    /// resolved for it, and is called again if the runtime restarts the process,
    /// so anything the saga captures (e.g. an `AggregateProxy` to tell) must be
    /// re-acquirable.
    ///
    /// # Typestate
    ///
    /// The store-less form exists only on a system that was given a default
    /// store.  Calling it on one that was not is a **compile error**:
    ///
    /// ```compile_fail
    /// # use async_trait::async_trait;
    /// # use bytes::Bytes;
    /// # use nitinol_eventsource::codec::Codec;
    /// # use nitinol_eventsource::system::EventSourceSystem;
    /// # use nitinol_eventsource::Event;
    /// # use nitinol_persistence::{EventType, Family, TypeName};
    /// # use nitinol_runtime::ProcessSystem;
    /// # use nitinol_saga::{Saga, SagaContext, SagaDefaultStoreExt, SagaEffect, SagaId};
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
    /// // no `with_event_store` — there is no default journal to spawn onto
    /// let system = EventSourceSystem::builder(ps).build();
    /// system.spawn_saga(SagaId::new("reservation"), |_| ReservationSaga);
    /// # }
    /// ```
    fn spawn_saga<S>(
        &self,
        saga_id: SagaId,
        producer: impl Fn(SagaComponents<S>) -> S + Send + Sync + 'static,
    ) -> SagaSpawn<'_, S>
    where
        S: Saga;

    /// Begin a saga spawn that journals onto `store`, overriding the system
    /// default.
    ///
    /// The default receives nothing of this saga's own stream — not its domain
    /// events, not its outbox markers, not the positions it consumed.  The
    /// sources it observes are chosen separately, by the [`Subscription`]s handed
    /// to [`SagaSpawn::subscribed_to`].
    fn spawn_saga_with_store<S>(
        &self,
        saga_id: SagaId,
        store: Arc<dyn EventStore>,
        producer: impl Fn(SagaComponents<S>) -> S + Send + Sync + 'static,
    ) -> SagaSpawn<'_, S>
    where
        S: Saga;

    /// Subscribe to the stream `key` in the system's default [`EventStore`],
    /// from its beginning.
    ///
    /// The store-less counterpart of [`Subscription::stream`], which names one
    /// explicitly and overrides the default.  The returned value is an ordinary
    /// [`Subscription`], so [`with_after`](Subscription::with_after) resumes
    /// from a position already processed.
    ///
    /// # Typestate
    ///
    /// Like [`spawn_saga`](Self::spawn_saga), this exists only on a system that
    /// was given a default store:
    ///
    /// ```compile_fail
    /// # use nitinol_eventsource::system::EventSourceSystem;
    /// # use nitinol_persistence::AggregateId;
    /// # use nitinol_runtime::ProcessSystem;
    /// # use nitinol_saga::SagaDefaultStoreExt;
    /// # async fn wiring() {
    /// # let ps = ProcessSystem::new().await;
    /// // no `with_event_store` — there is no default store to poll
    /// let system = EventSourceSystem::builder(ps).build();
    /// system.subscription(&AggregateId::new("order"));
    /// # }
    /// ```
    fn subscription<K>(&self, key: &K) -> Subscription
    where
        K: ?Sized + Borrow<str>;

    /// Return a [`SagaManagerProps`] journaling onto the system's default store.
    ///
    /// One default store holds the observed streams and every correlated
    /// instance's own stream side by side, keyed by the [`SagaId`]
    /// [`Saga::correlate`] names.  Which sources the manager observes stays the
    /// caller's choice, through [`SagaManagerProps::subscribed_to`]: that is also
    /// the override point, since [`Subscription::stream`] names a store where
    /// [`subscription`](Self::subscription) takes the default.  A manager whose
    /// instances must journal elsewhere is built from
    /// [`SagaManagerProps::new`] instead.
    ///
    /// # Typestate
    ///
    /// Like [`spawn_saga`](Self::spawn_saga), this exists only on a system that
    /// was given a default store:
    ///
    /// ```compile_fail
    /// # use async_trait::async_trait;
    /// # use bytes::Bytes;
    /// # use nitinol_eventsource::codec::Codec;
    /// # use nitinol_eventsource::system::EventSourceSystem;
    /// # use nitinol_eventsource::Event;
    /// # use nitinol_persistence::{EventType, Family, TypeName};
    /// # use nitinol_runtime::ProcessSystem;
    /// # use nitinol_saga::{Saga, SagaContext, SagaDefaultStoreExt, SagaEffect, SagaId};
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
    /// // no `with_event_store` — the instances have no default journal
    /// let system = EventSourceSystem::builder(ps).build();
    /// system.saga_manager_props(|_| ReservationSaga);
    /// # }
    /// ```
    fn saga_manager_props<S>(
        &self,
        producer: impl Fn(SagaComponents<S>) -> S + Send + Sync + 'static,
    ) -> SagaManagerProps<S>
    where
        S: Saga;
}

impl SagaDefaultStoreExt for EventSourceSystem<StoreSet> {
    fn spawn_saga<S>(
        &self,
        saga_id: SagaId,
        producer: impl Fn(SagaComponents<S>) -> S + Send + Sync + 'static,
    ) -> SagaSpawn<'_, S>
    where
        S: Saga,
    {
        self.spawn_saga_with_store(saga_id, Arc::clone(self.event_store()), producer)
    }

    fn spawn_saga_with_store<S>(
        &self,
        saga_id: SagaId,
        store: Arc<dyn EventStore>,
        producer: impl Fn(SagaComponents<S>) -> S + Send + Sync + 'static,
    ) -> SagaSpawn<'_, S>
    where
        S: Saga,
    {
        SagaSpawn::new(self.process_system(), saga_id, store, producer)
    }

    fn subscription<K>(&self, key: &K) -> Subscription
    where
        K: ?Sized + Borrow<str>,
    {
        Subscription::stream(self.event_store(), key)
    }

    fn saga_manager_props<S>(
        &self,
        producer: impl Fn(SagaComponents<S>) -> S + Send + Sync + 'static,
    ) -> SagaManagerProps<S>
    where
        S: Saga,
    {
        SagaManagerProps::<S>::new(Arc::clone(self.event_store()), producer)
    }
}

/// Builder returned by [`SagaSystemExt::spawn_saga`] and
/// [`SagaDefaultStoreExt::spawn_saga`].
///
/// `B` carries the binding typestate — one slot per source the saga's alphabet
/// declares — so [`spawn`](Self::spawn) exists only once
/// [`subscribed_to`](Self::subscribed_to) has filled every one of them.  The
/// requirement is enforced by the compiler rather than at spawn time.
///
/// The journal store is already settled by the entry point that produced this
/// builder, so which system state it came from no longer matters here: what is
/// left to carry is the `ProcessSystem` to spawn into.
pub struct SagaSpawn<'a, S: Saga, B = SourceBindings<S>> {
    ps: &'a ProcessSystem,
    props: SagaProps<S, B>,
}

impl<'a, S: Saga> SagaSpawn<'a, S, SourceBindings<S>> {
    fn new(
        ps: &'a ProcessSystem,
        saga_id: SagaId,
        store: Arc<dyn EventStore>,
        producer: impl Fn(SagaComponents<S>) -> S + Send + Sync + 'static,
    ) -> Self {
        SagaSpawn {
            ps,
            props: SagaProps::new(saga_id, store, producer),
        }
    }
}

impl<'a, S: Saga, B> SagaSpawn<'a, S, B> {
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
    ) -> SagaSpawn<'a, S, <B as BindSource<E>>::Bound>
    where
        E: Event,
        B: BindSource<E>,
    {
        SagaSpawn {
            ps: self.ps,
            props: self.props.subscribed_to::<E>(subscription),
        }
    }
}

impl<S: Saga, B> SagaSpawn<'_, S, B>
where
    B: AllSourcesSubscribed<S::Subscribe>,
{
    /// Spawn the saga into the system's `ProcessSystem`.
    ///
    /// Spawning before every declared source is bound is a **compile error** —
    /// including the degenerate case of binding nothing at all:
    ///
    /// ```compile_fail
    /// # use std::sync::Arc;
    /// # use async_trait::async_trait;
    /// # use bytes::Bytes;
    /// # use nitinol_eventsource::codec::Codec;
    /// # use nitinol_eventsource::system::EventSourceSystem;
    /// # use nitinol_eventsource::Event;
    /// # use nitinol_persistence::store::{EventStore, InMemoryEventStore};
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
    /// # let system = EventSourceSystem::builder(ps).build();
    /// # let saga_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    /// system
    ///     .spawn_saga(SagaId::new("reservation"), saga_store, |_| ReservationSaga)
    ///     .spawn()
    ///     .await;
    /// # }
    /// ```
    ///
    /// So is binding only *some* of the declared sources.  The alphabet below
    /// declares two, and the builder is handed one:
    ///
    /// ```compile_fail
    /// # use std::sync::Arc;
    /// # use async_trait::async_trait;
    /// # use bytes::Bytes;
    /// # use nitinol_eventsource::codec::Codec;
    /// # use nitinol_eventsource::system::EventSourceSystem;
    /// # use nitinol_eventsource::Event;
    /// # use nitinol_persistence::store::{EventStore, InMemoryEventStore};
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
    /// # let system = EventSourceSystem::builder(ps).build();
    /// # let order_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    /// # let order_id = AggregateId::new("order");
    /// # let saga_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    /// // `StockReserved` is declared and never bound, so `spawn` does not exist
    /// system
    ///     .spawn_saga(SagaId::new("reservation"), saga_store, |_| ReservationSaga)
    ///     .subscribed_to::<OrderPlaced>(Subscription::stream(&order_store, &order_id))
    ///     .spawn()
    ///     .await;
    /// # }
    /// ```
    ///
    /// Binding one source twice is a compile error too — the slot has already
    /// moved, so there is no transition left to apply:
    ///
    /// ```compile_fail
    /// # use std::sync::Arc;
    /// # use async_trait::async_trait;
    /// # use bytes::Bytes;
    /// # use nitinol_eventsource::codec::Codec;
    /// # use nitinol_eventsource::system::EventSourceSystem;
    /// # use nitinol_eventsource::Event;
    /// # use nitinol_persistence::store::{EventStore, InMemoryEventStore};
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
    /// # let system = EventSourceSystem::builder(ps).build();
    /// # let order_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    /// # let order_id = AggregateId::new("order");
    /// # let saga_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    /// system
    ///     .spawn_saga(SagaId::new("reservation"), saga_store, |_| ReservationSaga)
    ///     .subscribed_to::<OrderPlaced>(Subscription::stream(&order_store, &order_id))
    ///     .subscribed_to::<OrderPlaced>(Subscription::stream(&order_store, &order_id))
    ///     .spawn()
    ///     .await;
    /// # }
    /// ```
    ///
    /// And so is binding a source the alphabet never declared:
    ///
    /// ```compile_fail
    /// # use std::sync::Arc;
    /// # use async_trait::async_trait;
    /// # use bytes::Bytes;
    /// # use nitinol_eventsource::codec::Codec;
    /// # use nitinol_eventsource::system::EventSourceSystem;
    /// # use nitinol_eventsource::Event;
    /// # use nitinol_persistence::store::{EventStore, InMemoryEventStore};
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
    /// # let system = EventSourceSystem::builder(ps).build();
    /// # let order_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    /// # let order_id = AggregateId::new("order");
    /// # let saga_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    /// system
    ///     .spawn_saga(SagaId::new("reservation"), saga_store, |_| ReservationSaga)
    ///     .subscribed_to::<Undeclared>(Subscription::stream(&order_store, &order_id))
    ///     .spawn()
    ///     .await;
    /// # }
    /// ```
    pub async fn spawn(self) -> SagaProxy<S> {
        self.props.spawn(self.ps).await
    }
}
