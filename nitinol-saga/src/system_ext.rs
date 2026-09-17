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
use nitinol_eventsource::SequenceCursor;
use nitinol_persistence::store::EventStore;
use nitinol_runtime::ProcessSystem;

use crate::id::SagaId;
use crate::process::{SagaManagerProps, SagaProps, SagaProxy, SubscriptionSet, SubscriptionUnset};
use crate::saga::Saga;

/// Which upstream records a saga is fed: the store to poll and the position to
/// start from, as one value.
///
/// The two travel together — a position means nothing without the store it
/// indexes into — so the spawn builder takes them as a single argument rather
/// than as a store plus a hand-built cursor.
pub struct Subscription {
    store: Arc<dyn EventStore>,
    stream_key: String,
    after: u64,
}

impl Subscription {
    /// Subscribe to one upstream stream, from its beginning.
    ///
    /// `key` is the stream key the upstream events are appended under — an
    /// [`AggregateId`](nitinol_persistence::AggregateId) for an aggregate's
    /// stream, a [`SagaId`] for another saga's own stream.  Starting at the
    /// beginning means the saga catches up on everything already written before
    /// it was spawned; [`with_after`](Self::with_after) resumes from a position
    /// already processed instead.
    pub fn stream<K>(store: &Arc<dyn EventStore>, key: &K) -> Self
    where
        K: ?Sized + Borrow<str>,
    {
        Self {
            store: Arc::clone(store),
            stream_key: key.borrow().to_owned(),
            after: 0,
        }
    }

    /// Deliver only the records past sequence `after`, skipping the ones at or
    /// below it.
    pub fn with_after(mut self, after: u64) -> Self {
        self.after = after;
        self
    }

    fn into_parts(self) -> (Arc<dyn EventStore>, SequenceCursor) {
        (
            self.store,
            SequenceCursor::Stream {
                key: self.stream_key,
                after: self.after,
            },
        )
    }
}

/// Spawns a [`Saga`] from an [`EventSourceSystem`].
///
/// This is the saga counterpart of
/// [`EventSourceSystem::spawn_aggregate`]: the caller names the saga and its
/// own [`EventStore`], and the system supplies the `ProcessSystem` to spawn
/// into.  The formats are settled already — `S::Event` and `S::SubscribedEvent`
/// each name the codec that writes and reads them.
///
/// [`SagaProps`] remains the way to spawn a saga that needs the settings this
/// entry point does not take (a scheduler, a dead-letter subscriber, an enqueue
/// policy, a crash-restart factory, or a decode-failure route).
pub trait SagaSystemExt {
    /// Begin a saga spawn against this system.
    ///
    /// `producer` constructs the saga instance and is called again if the
    /// runtime restarts the process, so anything the saga captures (e.g. an
    /// `AggregateProxy` to tell) must be re-acquirable.
    ///
    /// The returned builder still needs a [`Subscription`] before it can be
    /// spawned:
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
    /// # struct ReservationSaga;
    /// # #[async_trait]
    /// # impl Saga for ReservationSaga {
    /// #     type SubscribedEvent = OrderPlaced;
    /// #     type Event = ReservationRequested;
    /// #     type ScheduledMessage = ();
    /// #     type Error = std::convert::Infallible;
    /// #     fn correlate(_: &OrderPlaced) -> Option<SagaId> { Some(SagaId::new("reservation")) }
    /// #     fn apply(&mut self, _: ReservationRequested) {}
    /// #     async fn handle(&mut self, _: OrderPlaced, _: &mut SagaContext)
    /// #         -> Result<SagaEffect<ReservationRequested>, Self::Error> { Ok(SagaEffect::None) }
    /// # }
    /// # async fn wiring() {
    /// # let ps = ProcessSystem::new().await;
    /// # let system = EventSourceSystem::builder(ps).build();
    /// # let order_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    /// # let order_id = AggregateId::new("order");
    /// # let saga_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    /// let saga_proxy = system
    ///     .spawn_saga(SagaId::new("reservation"), saga_store, || ReservationSaga)
    ///     .subscribed_to(Subscription::stream(&order_store, &order_id))
    ///     .spawn()
    ///     .await;
    /// # }
    /// ```
    fn spawn_saga<S: Saga>(
        &self,
        saga_id: SagaId,
        store: Arc<dyn EventStore>,
        producer: impl Fn() -> S + Send + Sync + 'static,
    ) -> SagaSpawn<'_, S>;
}

impl SagaSystemExt for EventSourceSystem<StoreUnset> {
    fn spawn_saga<S: Saga>(
        &self,
        saga_id: SagaId,
        store: Arc<dyn EventStore>,
        producer: impl Fn() -> S + Send + Sync + 'static,
    ) -> SagaSpawn<'_, S> {
        SagaSpawn::new(self.process_system(), saga_id, store, producer)
    }
}

/// Spawning a saga, a saga manager, or an upstream subscription from an
/// [`EventSourceSystem`] that carries a default [`EventStore`].
///
/// A saga touches two stores: the one it journals its own events onto, and the
/// one it polls for upstream records.  Both are the system's default here —
/// [`spawn_saga`](Self::spawn_saga) for the journal,
/// [`subscription`](Self::subscription) for the upstream — because
/// [`EventStore`] is stream-keyed and one instance holds the saga's own stream
/// next to the aggregate stream it reacts to.
///
/// Each direction keeps its own override, which takes precedence over the
/// default: [`spawn_saga_with_store`](Self::spawn_saga_with_store) names a
/// journal, [`Subscription::stream`] names an upstream.
///
/// This trait exists alongside [`SagaSystemExt`] rather than replacing it: a
/// system built without [`with_event_store`][nitinol_eventsource::system::EventSourceSystemBuilder::with_event_store]
/// has no default to resolve, so it keeps the store-per-spawn entry point and
/// none of the store-less ones.
pub trait SagaDefaultStoreExt {
    /// Begin a saga spawn that journals onto the system's default store.
    ///
    /// `producer` constructs the saga instance and is called again if the
    /// runtime restarts the process, so anything the saga captures (e.g. an
    /// `AggregateProxy` to tell) must be re-acquirable.
    ///
    /// The returned builder still needs a [`Subscription`] before it can be
    /// spawned:
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
    /// # struct ReservationSaga;
    /// # #[async_trait]
    /// # impl Saga for ReservationSaga {
    /// #     type SubscribedEvent = OrderPlaced;
    /// #     type Event = ReservationRequested;
    /// #     type ScheduledMessage = ();
    /// #     type Error = std::convert::Infallible;
    /// #     fn correlate(_: &OrderPlaced) -> Option<SagaId> { Some(SagaId::new("reservation")) }
    /// #     fn apply(&mut self, _: ReservationRequested) {}
    /// #     async fn handle(&mut self, _: OrderPlaced, _: &mut SagaContext)
    /// #         -> Result<SagaEffect<ReservationRequested>, Self::Error> { Ok(SagaEffect::None) }
    /// # }
    /// # async fn wiring() {
    /// # let ps = ProcessSystem::new().await;
    /// # let store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    /// let system = EventSourceSystem::builder(ps)
    ///     .with_event_store(store)
    ///     .build();
    /// let order_id = AggregateId::new("order");
    /// let saga_proxy = system
    ///     .spawn_saga(SagaId::new("reservation"), || ReservationSaga)
    ///     .subscribed_to(system.subscription(&order_id))
    ///     .spawn()
    ///     .await;
    /// # }
    /// ```
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
    /// # struct ReservationSaga;
    /// # #[async_trait]
    /// # impl Saga for ReservationSaga {
    /// #     type SubscribedEvent = OrderPlaced;
    /// #     type Event = ReservationRequested;
    /// #     type ScheduledMessage = ();
    /// #     type Error = std::convert::Infallible;
    /// #     fn correlate(_: &OrderPlaced) -> Option<SagaId> { Some(SagaId::new("reservation")) }
    /// #     fn apply(&mut self, _: ReservationRequested) {}
    /// #     async fn handle(&mut self, _: OrderPlaced, _: &mut SagaContext)
    /// #         -> Result<SagaEffect<ReservationRequested>, Self::Error> { Ok(SagaEffect::None) }
    /// # }
    /// # async fn wiring() {
    /// # let ps = ProcessSystem::new().await;
    /// // no `with_event_store` — there is no default journal to spawn onto
    /// let system = EventSourceSystem::builder(ps).build();
    /// system.spawn_saga(SagaId::new("reservation"), || ReservationSaga);
    /// # }
    /// ```
    fn spawn_saga<S>(
        &self,
        saga_id: SagaId,
        producer: impl Fn() -> S + Send + Sync + 'static,
    ) -> SagaSpawn<'_, S>
    where
        S: Saga;

    /// Begin a saga spawn that journals onto `store`, overriding the system
    /// default.
    ///
    /// The default receives nothing of this saga's own stream — neither its
    /// domain events nor its outbox markers.  The upstream it is subscribed to
    /// is chosen separately, by the [`Subscription`] handed to
    /// [`SagaSpawn::subscribed_to`].
    fn spawn_saga_with_store<S>(
        &self,
        saga_id: SagaId,
        store: Arc<dyn EventStore>,
        producer: impl Fn() -> S + Send + Sync + 'static,
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
    /// # use bytes::Bytes;
    /// # use nitinol_eventsource::codec::Codec;
    /// # use nitinol_eventsource::system::EventSourceSystem;
    /// # use nitinol_eventsource::Event;
    /// # use nitinol_persistence::{AggregateId, EventType, Family, TypeName};
    /// # use nitinol_runtime::ProcessSystem;
    /// # use nitinol_saga::SagaDefaultStoreExt;
    /// # struct DocCodec;
    /// # #[derive(Clone)] struct OrderPlaced;
    /// # impl Codec<OrderPlaced> for DocCodec {
    /// #     type Error = std::convert::Infallible;
    /// #     fn encode(_: &OrderPlaced) -> Result<Bytes, Self::Error> { Ok(Bytes::new()) }
    /// #     fn decode(_: &[u8]) -> Result<OrderPlaced, Self::Error> { Ok(OrderPlaced) }
    /// # }
    /// # impl Event for OrderPlaced {
    /// #     const EVENT_TYPE: EventType = EventType::new(Family::new("doc"), TypeName::new("OrderPlaced"));
    /// #     type Codec = DocCodec;
    /// # }
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

    /// Return a [`SagaManagerProps`] with everything the system already knows
    /// filled in: the store each instance journals onto, and the upstream it
    /// polls.
    ///
    /// One default store holds the upstream stream and every correlated
    /// instance's own stream side by side, keyed by the [`SagaId`]
    /// [`Saga::correlate`] names.  `subscription` stays the caller's choice, so
    /// it is also the manager's override point:
    /// [`Subscription::stream`] names an upstream store where
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
    /// # use std::sync::Arc;
    /// # use async_trait::async_trait;
    /// # use bytes::Bytes;
    /// # use nitinol_eventsource::codec::Codec;
    /// # use nitinol_eventsource::system::EventSourceSystem;
    /// # use nitinol_eventsource::Event;
    /// # use nitinol_persistence::store::{EventStore, InMemoryEventStore};
    /// # use nitinol_persistence::{AggregateId, EventType, Family, TypeName};
    /// # use nitinol_runtime::ProcessSystem;
    /// # use nitinol_saga::{Saga, SagaContext, SagaDefaultStoreExt, SagaEffect, SagaId, Subscription};
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
    /// # struct ReservationSaga;
    /// # #[async_trait]
    /// # impl Saga for ReservationSaga {
    /// #     type SubscribedEvent = OrderPlaced;
    /// #     type Event = ReservationRequested;
    /// #     type ScheduledMessage = ();
    /// #     type Error = std::convert::Infallible;
    /// #     fn correlate(_: &OrderPlaced) -> Option<SagaId> { Some(SagaId::new("reservation")) }
    /// #     fn apply(&mut self, _: ReservationRequested) {}
    /// #     async fn handle(&mut self, _: OrderPlaced, _: &mut SagaContext)
    /// #         -> Result<SagaEffect<ReservationRequested>, Self::Error> { Ok(SagaEffect::None) }
    /// # }
    /// # async fn wiring() {
    /// # let ps = ProcessSystem::new().await;
    /// # let upstream: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    /// # let order_id = AggregateId::new("order");
    /// // no `with_event_store` — the instances have no default journal
    /// let system = EventSourceSystem::builder(ps).build();
    /// system.saga_manager_props(Subscription::stream(&upstream, &order_id), || ReservationSaga);
    /// # }
    /// ```
    fn saga_manager_props<S>(
        &self,
        subscription: Subscription,
        producer: impl Fn() -> S + Send + Sync + 'static,
    ) -> SagaManagerProps<S, SubscriptionSet<S>>
    where
        S: Saga;
}

impl SagaDefaultStoreExt for EventSourceSystem<StoreSet> {
    fn spawn_saga<S>(
        &self,
        saga_id: SagaId,
        producer: impl Fn() -> S + Send + Sync + 'static,
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
        producer: impl Fn() -> S + Send + Sync + 'static,
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
        subscription: Subscription,
        producer: impl Fn() -> S + Send + Sync + 'static,
    ) -> SagaManagerProps<S, SubscriptionSet<S>>
    where
        S: Saga,
    {
        let (upstream_store, cursor) = subscription.into_parts();
        SagaManagerProps::<S>::new(Arc::clone(self.event_store()), producer)
            .with_subscription(upstream_store, cursor)
    }
}

/// Builder returned by [`SagaSystemExt::spawn_saga`] and
/// [`SagaDefaultStoreExt::spawn_saga`].
///
/// `Sub` carries the subscription state (`SubscriptionUnset` /
/// `SubscriptionSet<S>`): [`spawn`](Self::spawn) exists only once
/// [`subscribed_to`](Self::subscribed_to) has supplied one, so the requirement
/// is enforced by the compiler rather than at spawn time.
///
/// The journal store is already settled by the entry point that produced this
/// builder, so which system state it came from no longer matters here: what is
/// left to carry is the `ProcessSystem` to spawn into.
pub struct SagaSpawn<'a, S: Saga, Sub = SubscriptionUnset> {
    ps: &'a ProcessSystem,
    props: SagaProps<S, Sub>,
}

impl<'a, S: Saga> SagaSpawn<'a, S, SubscriptionUnset> {
    fn new(
        ps: &'a ProcessSystem,
        saga_id: SagaId,
        store: Arc<dyn EventStore>,
        producer: impl Fn() -> S + Send + Sync + 'static,
    ) -> Self {
        SagaSpawn {
            ps,
            props: SagaProps::new(saga_id, store, producer),
        }
    }

    /// Feed this saga the upstream records `subscription` selects, decoded with
    /// the codec [`Saga::SubscribedEvent`] names.
    ///
    /// Which of the delivered events this instance acts on is decided by
    /// [`Saga::correlate`], not here.
    pub fn subscribed_to(self, subscription: Subscription) -> SagaSpawn<'a, S, SubscriptionSet<S>> {
        let (upstream_store, cursor) = subscription.into_parts();
        SagaSpawn {
            ps: self.ps,
            props: self.props.with_subscription(upstream_store, cursor),
        }
    }
}

impl<S: Saga> SagaSpawn<'_, S, SubscriptionSet<S>> {
    /// Spawn the saga into the system's `ProcessSystem`.
    ///
    /// Spawning without [`subscribed_to`](SagaSpawn::subscribed_to) is a
    /// **compile error**:
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
    /// # struct ReservationSaga;
    /// # #[async_trait]
    /// # impl Saga for ReservationSaga {
    /// #     type SubscribedEvent = OrderPlaced;
    /// #     type Event = ReservationRequested;
    /// #     type ScheduledMessage = ();
    /// #     type Error = std::convert::Infallible;
    /// #     fn correlate(_: &OrderPlaced) -> Option<SagaId> { Some(SagaId::new("reservation")) }
    /// #     fn apply(&mut self, _: ReservationRequested) {}
    /// #     async fn handle(&mut self, _: OrderPlaced, _: &mut SagaContext)
    /// #         -> Result<SagaEffect<ReservationRequested>, Self::Error> { Ok(SagaEffect::None) }
    /// # }
    /// # async fn wiring() {
    /// # let ps = ProcessSystem::new().await;
    /// # let system = EventSourceSystem::builder(ps).build();
    /// # let saga_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    /// system
    ///     .spawn_saga(SagaId::new("reservation"), saga_store, || ReservationSaga)
    ///     .spawn()
    ///     .await;
    /// # }
    /// ```
    pub async fn spawn(self) -> SagaProxy<S> {
        self.props.spawn(self.ps).await
    }
}
