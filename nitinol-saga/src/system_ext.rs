//! Hosting a saga, and wiring a delivery host, from an [`EventSourceSystem`].
//!
//! # Why an extension trait
//!
//! `nitinol-saga` depends on `nitinol-eventsource`, so an inherent
//! `host` on [`EventSourceSystem`] would have to live in
//! `nitinol-eventsource` and make that crate depend back on this one.  Declaring
//! the traits here keeps the dependency arrow pointing `saga -> eventsource`:
//! they are local, so implementing them for the foreign [`EventSourceSystem`]
//! is allowed, and the public entry points they need —
//! [`EventSourceSystem::process_system`] and
//! [`EventSourceSystem::persistence`] — already exist.

use std::borrow::Borrow;

use nitinol_eventsource::system::{EventSourceSystem, Hosted};
use nitinol_persistence::provider::{SagaOffsetSlot, Store};
use nitinol_persistence::store::OffsetStore;

use crate::components::SagaComponents;
use crate::delivery::SagaDeliveryProps;
use crate::process::SagaManagerProps;
use crate::saga::Saga;
use crate::subscribe::SourceBindings;
use crate::subscription::Subscription;

/// Hosting a saga, or naming an upstream subscription, from an
/// [`EventSourceSystem`].
///
/// This is the saga counterpart of [`EventSourceSystem::spawn_aggregate`]: the
/// caller names the saga type, and the journal comes from the provider the
/// system was built with. The formats are settled already — every type in
/// `S::Subscribe` and `S::Event` names the codec that writes and reads it.
///
/// A saga touches two kinds of store: the one its instances journal their own
/// events onto, and the ones they poll for the sources they observe. Both are
/// the provider's — [`host`](Self::host) for the journal,
/// [`subscription`](Self::subscription) for a source — because
/// [`EventStore`](nitinol_persistence::store::EventStore) is stream-keyed and
/// one instance of it holds every saga instance's own stream next to the
/// aggregate streams they react to.
///
/// Observing a source in a *different* destination is [`Subscription::stream`],
/// which names a store; journalling into one is [`SagaManagerProps::new`] on
/// the store a second provider supplies. Both are explicit, which is what
/// reaching two destinations has to be.
pub trait SagaSystemExt {
    /// Host the saga `S`, journalling onto the store the provider supplies.
    ///
    /// The one verb a saga is started with.  It always starts a manager, and a
    /// saga whose [`Saga::correlate`] always answers the same
    /// [`SagaId`](crate::SagaId) is that manager with a single instance — the
    /// degenerate case rather than a second mechanism, so there is nothing
    /// else to reach for and nothing to choose between.
    ///
    /// `factory` builds a saga instance from the capabilities the framework
    /// resolved for it, and is called again for every instance the correlation
    /// fans out to and whenever the runtime rebuilds one, so anything the saga
    /// captures (a reference it reads the world through, say) must be
    /// re-acquirable.  A saga that needs no capability is built by
    /// `|_| S::default()`.
    ///
    /// What is left to say at the wiring site is the binding of each declared
    /// source and the instances' residency — one builder's worth:
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
    /// let saga = system
    ///     .host::<ReservationSaga>(|_| ReservationSaga)
    ///     .observing::<OrderPlaced>(system.subscription(&order_id))
    ///     .spawn()
    ///     .await;
    /// # }
    /// ```
    ///
    /// Leaving a declared source unbound is a compile error, which is stated
    /// where the requirement lives — on
    /// [`SagaManagerProps::spawn`](SagaManagerProps::spawn).
    fn host<S: Saga>(
        &self,
        factory: impl Fn(SagaComponents<S>) -> S + Send + Sync + 'static,
    ) -> SagaManagerProps<S, SourceBindings<S>, Hosted>;

    /// Subscribe to the stream `key` in the store the provider supplies, from
    /// its beginning.
    ///
    /// The counterpart of [`Subscription::stream`], which names a store
    /// explicitly and is how a source in another destination is observed.  The
    /// returned value is an ordinary [`Subscription`], so
    /// [`with_after`](Subscription::with_after) starts past a position and
    /// [`with_poll_interval`](Subscription::with_poll_interval) names how often
    /// this source is asked.
    fn subscription<K>(&self, key: &K) -> Subscription
    where
        K: ?Sized + Borrow<str>;
}

impl<St: Store> SagaSystemExt for EventSourceSystem<St> {
    fn host<S: Saga>(
        &self,
        factory: impl Fn(SagaComponents<S>) -> S + Send + Sync + 'static,
    ) -> SagaManagerProps<S, SourceBindings<S>, Hosted> {
        SagaManagerProps::<S>::new(self.persistence().event_store(), factory)
            .hosted(self.erase_store())
    }

    fn subscription<K>(&self, key: &K) -> Subscription
    where
        K: ?Sized + Borrow<str>,
    {
        Subscription::stream(&self.persistence().event_store(), key)
    }
}

/// Wiring a delivery host from an [`EventSourceSystem`].
///
/// A host needs two slots — the journal the saga's decisions are on, and the
/// saga-offset slot its progress is recorded in — and this resolves
/// both from the provider the system was built with.
///
/// It is a trait of its own rather than part of [`SagaSystemExt`] because the
/// saga slot is one a provider need not supply: a deployment that wires no
/// delivery host owes no store for one, and one that does is told so while the
/// wiring is compiled.
pub trait SagaDeliveryExt {
    /// The backend's own offset store, as the provider's saga slot is
    /// configured with it.
    type SagaOffsets: OffsetStore + 'static;

    /// Deliver what the saga `S` decided, through a host over this system.
    ///
    /// The counterpart of [`SagaSystemExt::host`]: that one starts the saga,
    /// this one starts the host that carries out what it decided.
    ///
    /// # Typestate
    ///
    /// This exists only where the provider supplies saga offsets.
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
    /// // no `saga_offsets` — this store has no saga slot
    /// let store = CustomStore::builder().eventstore(InMemoryEventStore::default()).build();
    /// let system = EventSourceSystem::builder(ps)
    ///     .with_persistence(PersistenceProvider::new(store))
    ///     .build();
    /// system.deliver::<ReservationSaga>();
    /// # }
    /// ```
    fn deliver<S: Saga>(&self) -> SagaDeliveryProps<S, Self::SagaOffsets, Hosted>;
}

impl<St: SagaOffsetSlot> SagaDeliveryExt for EventSourceSystem<St> {
    type SagaOffsets = St::SagaOffsets;

    fn deliver<S: Saga>(&self) -> SagaDeliveryProps<S, Self::SagaOffsets, Hosted> {
        SagaDeliveryProps::new(self.persistence()).hosted(self.erase_store())
    }
}
