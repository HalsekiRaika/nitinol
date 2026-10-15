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

use std::borrow::Borrow;

use nitinol_eventsource::system::EventSourceSystem;
use nitinol_persistence::provider::{SagaOffsetSlot, Store};
use nitinol_persistence::store::OffsetStore;

use crate::components::SagaComponents;
use crate::delivery::SagaDeliveryProps;
use crate::process::{SagaHosted, SagaManagerProps};
use crate::saga::Saga;
use crate::subscribe::SourceBindings;
use crate::subscription::Subscription;

/// Hosting a saga, or naming an upstream subscription, from an
/// [`EventSourceSystem`].
///
/// This is the saga counterpart of [`EventSourceSystem::spawn_aggregate`]: the
/// caller names the saga type, and the stores come from the provider the
/// system was built with. The formats are settled already — every type in
/// `S::Subscribe` and `S::Event` names the codec that writes and reads it.
///
/// A saga touches three slots: the journal its instances persist their own
/// events onto, the stores they poll for the sources they observe, and the one
/// its delivery host records how far it has carried those decisions in. All
/// are the provider's — [`host`](Self::host) for the journal and the progress
/// slot, [`subscription`](SagaSubscriptionExt::subscription) for a source —
/// because [`EventStore`](nitinol_persistence::store::EventStore) is
/// stream-keyed and one instance of it holds every saga instance's own stream
/// next to the aggregate streams they react to.
///
/// `subscription` is a separate trait, [`SagaSubscriptionExt`], rather than a
/// second method here: it reads the journal alone, so a provider with no
/// saga-offset slot — which has no [`host`](Self::host) — still has
/// `subscription`. See [`SagaSubscriptionExt`] for why that independence is
/// load-bearing rather than cosmetic.
///
/// Observing a source in a *different* destination is [`Subscription::stream`],
/// which names a store; journalling into one is [`SagaManagerProps::new`] on
/// the store a second provider supplies, with
/// [`SagaDeliveryProps::new`] naming the two a host reads and records in. Both
/// are explicit, which is what reaching two destinations has to be.
pub trait SagaSystemExt {
    /// The backend's own offset store, as the provider's saga slot is
    /// configured with it.
    ///
    /// `Clone` because a delivery host holds the handle by value and names it
    /// once per partition: distribution is the backend's own clone, which is
    /// what `PRV-1` has the store keep.
    type SagaOffsets: OffsetStore + Clone + 'static;

    /// Host the saga `S` on the stores the provider supplies — the manager,
    /// and the delivery host that carries out what it decides.
    ///
    /// The one verb a saga is started with.  It always starts a manager, and a
    /// saga whose [`Saga::correlate`] always answers the same
    /// [`SagaId`](crate::SagaId) is that manager with a single instance — the
    /// degenerate case rather than a second mechanism, so there is nothing
    /// else to reach for and nothing to choose between.
    ///
    /// # Why one verb starts both
    ///
    /// A manager journals what it decided and nothing more: the commands a
    /// decision implies reach their targets only because a delivery host reads
    /// that journal ([`SagaDeliveryProps`] is where that contract is written
    /// down).  A saga whose decisions nobody carries out is not a half-wired
    /// saga but a system that runs, looks alive and does nothing — which
    /// neither the type system nor the runtime could report while the two were
    /// declared separately.  So there is one declaration, and the delivery
    /// host's settings are named on the builder it hands back:
    /// [`with_delivery_id`](SagaManagerProps::with_delivery_id),
    /// [`with_partitions`](SagaManagerProps::with_partitions) and
    /// [`with_delivery_poll_interval`](SagaManagerProps::with_delivery_poll_interval).
    /// The handle [`spawn`](SagaManagerProps::spawn) answers with reaches both
    /// halves — see [`SagaHostProxy`](crate::SagaHostProxy).
    ///
    /// # Typestate
    ///
    /// Because a hosted saga always starts a host, this exists only where the
    /// provider supplies the saga-offset slot that host records its progress
    /// in.  A provider that supplies none is refused here, while the wiring is
    /// compiled, rather than when the host first tries to record where it got
    /// to — and rather than by quietly keeping the progress somewhere a
    /// restart cannot find it, which would weaken the at-least-once guarantee
    /// without saying so:
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
    /// # use nitinol_saga::{
    /// #     Saga, SagaContext, SagaEffect, SagaId, SagaSubscriptionExt, SagaSystemExt, Subscription,
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
    ) -> SagaManagerProps<S, SourceBindings<S>, SagaHosted<S, Self::SagaOffsets>>;
}

impl<St: SagaOffsetSlot> SagaSystemExt for EventSourceSystem<St> {
    type SagaOffsets = St::SagaOffsets;

    fn host<S: Saga>(
        &self,
        factory: impl Fn(SagaComponents<S>) -> S + Send + Sync + 'static,
    ) -> SagaManagerProps<S, SourceBindings<S>, SagaHosted<S, Self::SagaOffsets>> {
        SagaManagerProps::<S>::new(self.persistence().event_store(), factory).hosted(
            self.erase_store(),
            SagaDeliveryProps::new(self.persistence()),
        )
    }
}

/// Naming "the upstream a saga hosted on this provider observes," from an
/// [`EventSourceSystem`].
///
/// A separate trait from [`SagaSystemExt`], bound to [`Store`] rather than to
/// [`SagaOffsetSlot`], because reading the provider's journal back for a
/// subscription needs no saga-offset slot at all —
/// [`PersistenceProvider::event_store`](nitinol_persistence::provider::PersistenceProvider::event_store)
/// is the only accessor [`subscription`](Self::subscription) calls, and it is
/// available on any [`Store`].
///
/// # Why this is not bundled into `SagaSystemExt`
///
/// A provider that supplies a journal but no saga-offset slot has no
/// [`host`](SagaSystemExt::host) — there is nowhere for a delivery host to
/// record its progress — but its journal is still a legitimate place to read
/// an upstream source from: a saga hosted on a *different* provider (one that
/// does supply the slot) can still declare a source living in this one, named
/// through `subscription`. Requiring the slot here as well would refuse that
/// case for a reason that has nothing to do with it, so the two methods are
/// two traits with two bounds instead of one trait inheriting the stricter of
/// the two.
pub trait SagaSubscriptionExt {
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

impl<St: Store> SagaSubscriptionExt for EventSourceSystem<St> {
    fn subscription<K>(&self, key: &K) -> Subscription
    where
        K: ?Sized + Borrow<str>,
    {
        Subscription::stream(self.persistence().event_store(), key)
    }
}
