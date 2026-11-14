//! Hosting a saga from an [`EventSourceSystem`].
//!
//! # Why an extension trait
//!
//! `nitinol-saga` depends on `nitinol-eventsource`, so an inherent
//! `host` on [`EventSourceSystem`] would have to live in
//! `nitinol-eventsource` and make that crate depend back on this one.  Declaring
//! the trait here keeps the dependency arrow pointing `saga -> eventsource`:
//! it is local, so implementing it for the foreign [`EventSourceSystem`]
//! is allowed, and the public entry points it needs —
//! [`EventSourceSystem::process_system`] and
//! [`EventSourceSystem::persistence`] — already exist.

use nitinol_eventsource::system::EventSourceSystem;
use nitinol_persistence::provider::SagaOffsetSlot;

use crate::components::SagaComponents;
use crate::process::{SagaHosted, SagaManagerProps};
use crate::saga::Saga;
use crate::subscribe::SourceBindings;

/// Hosting a saga from an [`EventSourceSystem`].
///
/// This is the saga counterpart of [`EventSourceSystem::spawn_aggregate`]: the
/// caller names the saga type, and the stores come from the supplier the
/// system was built with. The formats are settled already — every type in
/// `S::Subscribe` and `S::Event` names the codec that writes and reads it.
///
/// A saga touches three slots, and all three are the system's: the journal its
/// instances persist their own events onto, the journal they poll for the
/// sources they observe, and the slot its delivery host records how far it has
/// carried those decisions in. Nothing is named at the wiring, because
/// [`EventStore`](nitinol_persistence::store::EventStore) is stream-keyed and
/// one instance of it holds every saga instance's own stream next to the
/// aggregate streams they react to — so one supplier is enough and a
/// [`Subscription`](crate::Subscription) is a stream key and a position.
///
/// Observing a source in a *different* destination, or journalling into one, is
/// a saga hosted on a second system over the supplier that holds it. That is
/// explicit, which is what reaching two destinations has to be.
pub trait SagaSystemExt {
    /// This system's own store type, as the props it hands back carry it.
    ///
    /// Named here rather than erased so the saga-offset slot a delivery host
    /// records its progress in stays a requirement of the returned props'
    /// [`spawn`](SagaManagerProps::spawn), the same way it is a requirement of
    /// the props-first form.
    type Store: SagaOffsetSlot;

    /// Host the saga `S` on the stores the supplier distributes: the manager,
    /// the delivery host that carries out what it decides, and, when `S`'s
    /// alphabet declares a deadline, the deadline index.
    ///
    /// The system-first form of
    /// [`SagaManagerProps::new`](SagaManagerProps::new)`(..).spawn(&system)`;
    /// the delivery host's settings are named on the props this returns.
    /// `factory` is called for every instance and whenever the runtime rebuilds
    /// one, so anything the saga captures must be re-acquirable.
    ///
    /// This exists only where the store holds the saga-offset slot the delivery
    /// host records its progress in; a store without one is a **compile error**:
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
    /// // no `saga_offsets` — this store has no saga slot
    /// let store = CustomStore::builder().eventstore(InMemoryEventStore::default()).build();
    /// let system = EventSourceSystem::builder(ps)
    ///     .with_persistence(PersistenceProvider::new(store))
    ///     .build();
    /// system.host::<ReservationSaga>(|_| ReservationSaga);
    /// # }
    /// ```
    ///
    /// The wiring site binds each declared source, then spawns:
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
    ///     .observing::<OrderPlaced>(Subscription::stream(&order_id))
    ///     .spawn()
    ///     .await;
    /// # }
    /// ```
    fn host<S: Saga>(
        &self,
        factory: impl Fn(SagaComponents<S>) -> S + Send + Sync + 'static,
    ) -> SagaManagerProps<S, SourceBindings<S>, SagaHosted<S, Self::Store>>;
}

impl<St: SagaOffsetSlot> SagaSystemExt for EventSourceSystem<St> {
    type Store = St;

    fn host<S: Saga>(
        &self,
        factory: impl Fn(SagaComponents<S>) -> S + Send + Sync + 'static,
    ) -> SagaManagerProps<S, SourceBindings<S>, SagaHosted<S, St>> {
        SagaManagerProps::<S>::new(factory).hosted(self.clone())
    }
}
