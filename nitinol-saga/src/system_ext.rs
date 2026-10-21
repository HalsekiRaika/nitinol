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

    /// Host the saga `S` on the stores the supplier distributes — the manager,
    /// and the delivery host that carries out what it decides.
    ///
    /// The system-first form of the one verb a saga is started with; the
    /// props-first form is
    /// [`SagaManagerProps::new`](SagaManagerProps::new)`(..).spawn(&system)`,
    /// and the two reach the same private path.  It always starts a manager,
    /// and a saga whose [`Saga::correlate`] always answers the same
    /// [`SagaId`](crate::SagaId) is that manager with a single instance — the
    /// degenerate case rather than a second mechanism, so there is nothing
    /// else to reach for and nothing to choose between.
    ///
    /// # Why one verb starts both
    ///
    /// A manager journals what it decided and nothing more: the commands a
    /// decision implies reach their targets only because a delivery host reads
    /// that journal ([`SagaDeliveryProps`](crate::SagaDeliveryProps) is where
    /// that contract is written down).  A saga whose decisions nobody carries
    /// out is not a half-wired saga but a system that runs, looks alive and
    /// does nothing — which neither the type system nor the runtime could
    /// report while the two were declared separately.  So there is one
    /// declaration, and the delivery host's settings are named on the builder
    /// it hands back:
    /// [`with_delivery_id`](SagaManagerProps::with_delivery_id),
    /// [`with_partitions`](SagaManagerProps::with_partitions) and
    /// [`with_delivery_poll_interval`](SagaManagerProps::with_delivery_poll_interval).
    /// The handle [`spawn`](SagaManagerProps::spawn) answers with reaches both
    /// halves — see [`SagaHostProxy`](crate::SagaHostProxy).
    ///
    /// # Typestate
    ///
    /// Because a hosted saga always starts a host, the props this hands back
    /// spawn only where the store supplies the saga-offset slot that host
    /// records its progress in.  A store that supplies none is refused while
    /// the wiring is compiled, rather than when the host first tries to record
    /// where it got to — and rather than by quietly keeping the progress
    /// somewhere a restart cannot find it, which would weaken the at-least-once
    /// guarantee without saying so:
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
    ///     .observing::<OrderPlaced>(Subscription::stream(&order_id))
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
