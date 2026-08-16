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
//! [`EventSourceSystem`] is allowed, and the two public entry points it needs
//! — [`EventSourceSystem::codec`] and [`EventSourceSystem::process_system`] —
//! already exist.

use std::borrow::Borrow;
use std::sync::Arc;

use nitinol_eventsource::codec::Codec;
use nitinol_eventsource::system::EventSourceSystem;
use nitinol_eventsource::SequenceCursor;
use nitinol_persistence::store::EventStore;

use crate::id::SagaId;
use crate::process::{CodecSet, SagaProps, SagaProxy, SubscriptionSet, SubscriptionUnset};
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

/// Spawns a [`Saga`] from an [`EventSourceSystem`], resolving both of its
/// codecs from the system's codec type.
///
/// This is the saga counterpart of
/// [`EventSourceSystem::spawn_aggregate`]: the caller names the saga and its
/// own [`EventStore`], and the system supplies what it already knows — the
/// codec for the saga's own events, the codec for the events it subscribes to,
/// and the `ProcessSystem` to spawn into.
///
/// [`SagaProps`] remains the way to spawn a saga that needs the settings this
/// entry point does not take (a scheduler, a dead-letter subscriber, an enqueue
/// policy, a crash-restart factory, or a decode-failure route).
pub trait SagaSystemExt {
    /// The codec type the system resolves every event codec from.
    type Codec;

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
    /// # #[derive(Default)] struct DocCodec;
    /// # #[derive(Clone)] struct OrderPlaced;
    /// # #[derive(Clone)] struct ReservationRequested;
    /// # impl Event for OrderPlaced { const EVENT_TYPE: EventType = EventType::new(Family::new("doc"), TypeName::new("OrderPlaced")); }
    /// # impl Event for ReservationRequested { const EVENT_TYPE: EventType = EventType::new(Family::new("doc"), TypeName::new("ReservationRequested")); }
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
    /// # let system = EventSourceSystem::new(ps).with_codec::<DocCodec>().build();
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
    fn spawn_saga<S>(
        &self,
        saga_id: SagaId,
        store: Arc<dyn EventStore>,
        producer: impl Fn() -> S + Send + Sync + 'static,
    ) -> SagaSpawn<'_, Self::Codec, S>
    where
        S: Saga,
        Self::Codec: Codec<S::Event>;
}

impl<C> SagaSystemExt for EventSourceSystem<C>
where
    C: Default + Send + Sync + 'static,
{
    type Codec = C;

    fn spawn_saga<S>(
        &self,
        saga_id: SagaId,
        store: Arc<dyn EventStore>,
        producer: impl Fn() -> S + Send + Sync + 'static,
    ) -> SagaSpawn<'_, C, S>
    where
        S: Saga,
        C: Codec<S::Event>,
    {
        SagaSpawn {
            system: self,
            props: SagaProps::new(saga_id, store, producer).with_codec(self.codec::<S::Event>()),
        }
    }
}

/// Builder returned by [`SagaSystemExt::spawn_saga`].
///
/// `Sub` carries the subscription state (`SubscriptionUnset` /
/// `SubscriptionSet<S>`): [`spawn`](Self::spawn) exists only once
/// [`subscribed_to`](Self::subscribed_to) has supplied one, so the requirement
/// is enforced by the compiler rather than at spawn time.  The codec state
/// [`SagaProps`] carries is not a parameter here — the system resolves both
/// codecs, so there is no unset codec to reach `spawn` with.
pub struct SagaSpawn<'a, C, S: Saga, Sub = SubscriptionUnset> {
    system: &'a EventSourceSystem<C>,
    props: SagaProps<S, CodecSet<S::Event>, Sub>,
}

impl<'a, C, S> SagaSpawn<'a, C, S, SubscriptionUnset>
where
    C: Codec<S::SubscribedEvent> + Default + Send + Sync + 'static,
    S: Saga,
{
    /// Feed this saga the upstream records `subscription` selects, decoded with
    /// the codec the system resolves for [`Saga::SubscribedEvent`].
    ///
    /// Which of the delivered events this instance acts on is decided by
    /// [`Saga::correlate`], not here.
    pub fn subscribed_to(
        self,
        subscription: Subscription,
    ) -> SagaSpawn<'a, C, S, SubscriptionSet<S>> {
        let upstream_codec = self.system.codec::<S::SubscribedEvent>();
        let (upstream_store, cursor) = subscription.into_parts();
        SagaSpawn {
            system: self.system,
            props: self
                .props
                .with_subscription(upstream_store, upstream_codec, cursor),
        }
    }
}

impl<C, S: Saga> SagaSpawn<'_, C, S, SubscriptionSet<S>> {
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
    /// # #[derive(Default)] struct DocCodec;
    /// # #[derive(Clone)] struct OrderPlaced;
    /// # #[derive(Clone)] struct ReservationRequested;
    /// # impl Event for OrderPlaced { const EVENT_TYPE: EventType = EventType::new(Family::new("doc"), TypeName::new("OrderPlaced")); }
    /// # impl Event for ReservationRequested { const EVENT_TYPE: EventType = EventType::new(Family::new("doc"), TypeName::new("ReservationRequested")); }
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
    /// # let system = EventSourceSystem::new(ps).with_codec::<DocCodec>().build();
    /// # let saga_store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    /// system
    ///     .spawn_saga(SagaId::new("reservation"), saga_store, || ReservationSaga)
    ///     .spawn()
    ///     .await;
    /// # }
    /// ```
    pub async fn spawn(self) -> SagaProxy<S> {
        self.props.spawn(self.system.process_system()).await
    }
}
