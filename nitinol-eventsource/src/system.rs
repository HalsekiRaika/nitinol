use std::marker::PhantomData;
use std::sync::Arc;

use nitinol_persistence::store::EventStore;
use nitinol_persistence::AggregateId;
use nitinol_runtime::ProcessSystem;

use crate::aggregate::Aggregate;
use crate::codec::{Codec, ErasedCodec};
use crate::{AggregateProps, AggregateProxy};

/// Marker type indicating no codec has been configured yet.
///
/// `EventSourceSystem<Unset>` has no useful methods — use
/// [`EventSourceSystem::new`] followed by [`EventSourceSystemBuilder::with_codec`]
/// to produce a configured system.
pub struct Unset;

/// Marker type indicating the system holds no default [`EventStore`].
///
/// Every spawn against a system in this state names its own store, exactly as
/// before [`EventSourceSystemBuilder::with_event_store`] existed.
pub struct StoreUnset;

/// Marker type indicating the system holds the default [`EventStore`] it was
/// built with, and carries it.
///
/// This is the state in which the store-less
/// [`spawn_aggregate`][EventSourceSystem::spawn_aggregate] exists: a spawn that
/// names no store is served from here.
pub struct StoreSet {
    store: Arc<dyn EventStore>,
}

/// Builder for [`EventSourceSystem`].
///
/// Obtained from [`EventSourceSystem::new`]. Call [`with_codec`][Self::with_codec]
/// to bind a codec type, optionally [`with_event_store`][Self::with_event_store]
/// to bind a default store, then [`build`][EventSourceSystemBuilder::build] to
/// produce an [`EventSourceSystem`].
///
/// # Builder
/// ```ignore
/// let system = EventSourceSystem::new(ps)
///     .with_codec::<JsonCodec>()
///     .with_event_store(store)
///     .build();
/// ```
pub struct EventSourceSystemBuilder<C = Unset, St = StoreUnset> {
    ps: ProcessSystem,
    store: St,
    _codec: PhantomData<C>,
}

impl EventSourceSystemBuilder<Unset, StoreUnset> {
    fn new(ps: ProcessSystem) -> Self {
        Self {
            ps,
            store: StoreUnset,
            _codec: PhantomData,
        }
    }
}

impl<St> EventSourceSystemBuilder<Unset, St> {
    /// Bind codec type `C` to this builder.
    pub fn with_codec<C>(self) -> EventSourceSystemBuilder<C, St> {
        EventSourceSystemBuilder {
            ps: self.ps,
            store: self.store,
            _codec: PhantomData,
        }
    }
}

impl<C> EventSourceSystemBuilder<C, StoreUnset> {
    /// Bind `store` as the system's default [`EventStore`].
    ///
    /// # One store, every stream
    ///
    /// [`EventStore`] is stream-keyed — `append(key, ..)` and
    /// [`LoadQuery::by_stream`](nitinol_persistence::LoadQuery::by_stream) address one
    /// stream at a time — so a single instance holds every aggregate's stream (and
    /// every saga's journal) side by side, each owner still reading only its own
    /// key.  That is what makes *one* default store sufficient rather than a
    /// registry of named ones: splitting streams across store instances is a
    /// deployment choice, and the per-spawn override below is how it is expressed.
    ///
    /// # Default and override
    ///
    /// Once a default is bound, the store argument disappears from the spawn
    /// entry points — [`spawn_aggregate`][EventSourceSystem::spawn_aggregate] and
    /// [`aggregate_props`][EventSourceSystem::aggregate_props] resolve it here
    /// instead of at every call site.  A spawn that must live elsewhere names its
    /// own store through
    /// [`spawn_aggregate_with_store`][EventSourceSystem::spawn_aggregate_with_store]
    /// or
    /// [`aggregate_props_with_store`][EventSourceSystem::aggregate_props_with_store],
    /// which take precedence: the default receives nothing for that spawn.
    pub fn with_event_store(
        self,
        store: Arc<dyn EventStore>,
    ) -> EventSourceSystemBuilder<C, StoreSet> {
        EventSourceSystemBuilder {
            ps: self.ps,
            store: StoreSet { store },
            _codec: PhantomData,
        }
    }
}

impl<C, St> EventSourceSystemBuilder<C, St> {
    /// Finalize construction and return the configured [`EventSourceSystem`].
    pub fn build(self) -> EventSourceSystem<C, St> {
        EventSourceSystem {
            ps: self.ps,
            store: self.store,
            _codec: PhantomData,
        }
    }
}

/// System-level orchestrator that binds a [`ProcessSystem`] to a codec type `C`,
/// and optionally to a default [`EventStore`].
///
/// The codec type is fixed at build time via [`EventSourceSystemBuilder::with_codec`]
/// and must implement [`Codec<E>`] for every event and snapshot type used through
/// this system.
///
/// `St` records whether [`EventSourceSystemBuilder::with_event_store`] was
/// called.  It decides which spawn entry points exist, so a store that was never
/// configured cannot be relied on:
///
/// | State | Spawn entry points |
/// |---|---|
/// | [`StoreUnset`] (the default) | [`spawn_aggregate(id, store)`][Self::spawn_aggregate] / [`aggregate_props(id, store)`][Self::aggregate_props] — the store is named per spawn |
/// | [`StoreSet`] | [`spawn_aggregate(id)`][Self::spawn_aggregate] / [`aggregate_props(id)`][Self::aggregate_props] take the default; [`spawn_aggregate_with_store`][Self::spawn_aggregate_with_store] / [`aggregate_props_with_store`][Self::aggregate_props_with_store] override it |
pub struct EventSourceSystem<C, St = StoreUnset> {
    ps: ProcessSystem,
    store: St,
    _codec: PhantomData<C>,
}

impl EventSourceSystem<Unset> {
    /// Begin building an [`EventSourceSystem`] backed by the given [`ProcessSystem`].
    ///
    /// Returns an [`EventSourceSystemBuilder`] — intentional factory method.
    #[allow(clippy::new_ret_no_self)]
    pub fn new(ps: ProcessSystem) -> EventSourceSystemBuilder<Unset, StoreUnset> {
        EventSourceSystemBuilder::new(ps)
    }
}

impl<C, St> EventSourceSystem<C, St> {
    /// Returns a reference to the underlying [`ProcessSystem`].
    pub fn process_system(&self) -> &ProcessSystem {
        &self.ps
    }
}

impl<C> EventSourceSystem<C, StoreSet> {
    /// The default [`EventStore`] this system was built with.
    ///
    /// Wiring that takes a store as a value — a
    /// [`ProjectorProps`](crate::ProjectorProps), an upstream subscription —
    /// reads it from here rather than keeping a second `Arc` alongside the
    /// system.
    pub fn event_store(&self) -> &Arc<dyn EventStore> {
        &self.store.store
    }
}

impl<C, St> EventSourceSystem<C, St>
where
    C: Default + Send + Sync + 'static,
{
    /// Return a type-erased codec for type `E`.
    ///
    /// Creates a default-constructed `C` and wraps it as `Arc<dyn ErasedCodec<E>>`.
    pub fn codec<E>(&self) -> Arc<dyn ErasedCodec<E>>
    where
        C: Codec<E>,
        E: Send + Sync + 'static,
    {
        Arc::new(C::default())
    }

    /// [`AggregateProps`] for `id` on `store`, pre-wired with the system codec.
    ///
    /// The one place a store and the system's codec are joined into props, so
    /// the default and the override resolve to the same wiring.
    fn props_on<A>(
        &self,
        id: AggregateId,
        store: Arc<dyn EventStore>,
    ) -> AggregateProps<A, crate::CodecSet<A::Event>>
    where
        A: Aggregate,
        C: Codec<A::Event>,
    {
        AggregateProps::<A>::new(id, store).with_codec(self.codec::<A::Event>())
    }
}

impl<C> EventSourceSystem<C, StoreUnset>
where
    C: Default + Send + Sync + 'static,
{
    /// Spawn an aggregate process on `store`, pre-wired with the system event codec.
    ///
    /// This system holds no default store, so the store is named here.  Binding
    /// one with [`EventSourceSystemBuilder::with_event_store`] makes the argument
    /// unnecessary — see [`EventSourceSystem<C, StoreSet>`][EventSourceSystem].
    pub async fn spawn_aggregate<A>(
        &self,
        id: AggregateId,
        store: Arc<dyn EventStore>,
    ) -> AggregateProxy<A>
    where
        A: Aggregate,
        C: Codec<A::Event>,
    {
        self.props_on::<A>(id, store).spawn(&self.ps).await
    }

    /// Return an [`AggregateProps`] on `store`, pre-wired with the system event codec.
    ///
    /// Use this when additional configuration (e.g., snapshot persistor) is needed
    /// before spawning.
    pub fn aggregate_props<A>(
        &self,
        id: AggregateId,
        store: Arc<dyn EventStore>,
    ) -> AggregateProps<A, crate::CodecSet<A::Event>>
    where
        A: Aggregate,
        C: Codec<A::Event>,
    {
        self.props_on::<A>(id, store)
    }
}

impl<C> EventSourceSystem<C, StoreSet>
where
    C: Default + Send + Sync + 'static,
{
    /// Spawn an aggregate process onto the system's default [`EventStore`],
    /// pre-wired with the system event codec.
    ///
    /// The aggregate's stream lives under `id` in the default store; other
    /// aggregates, and any saga journalling through this system, are tenants of
    /// the same instance under their own keys.
    ///
    /// To place this one aggregate elsewhere, name its store with
    /// [`spawn_aggregate_with_store`][Self::spawn_aggregate_with_store].
    ///
    /// # Typestate
    ///
    /// This store-less form exists only once
    /// [`with_event_store`][EventSourceSystemBuilder::with_event_store] has
    /// supplied a default.  Calling it on a system built without one is a
    /// **compile error** rather than a spawn-time failure:
    ///
    /// ```compile_fail
    /// # use std::sync::Arc;
    /// # use bytes::Bytes;
    /// # use nitinol_eventsource::codec::Codec;
    /// # use nitinol_eventsource::system::EventSourceSystem;
    /// # use nitinol_eventsource::{Aggregate, Event};
    /// # use nitinol_persistence::{AggregateId, EventType, Family, TypeName};
    /// # use nitinol_runtime::ProcessSystem;
    /// # #[derive(Default)] struct DocCodec;
    /// # #[derive(Clone)] struct Bumped;
    /// # impl Event for Bumped { const EVENT_TYPE: EventType = EventType::new(Family::new("doc"), TypeName::new("Bumped")); }
    /// # impl Codec<Bumped> for DocCodec {
    /// #     type Error = std::convert::Infallible;
    /// #     fn encode(_: &Bumped) -> Result<Bytes, Self::Error> { Ok(Bytes::new()) }
    /// #     fn decode(_: &[u8]) -> Result<Bumped, Self::Error> { Ok(Bumped) }
    /// # }
    /// # #[derive(Default)] struct Counter;
    /// # impl Aggregate for Counter { type Event = Bumped; fn apply(&mut self, _: Bumped) {} }
    /// # async fn wiring() {
    /// # let ps = ProcessSystem::new().await;
    /// // no `with_event_store` — there is no default store to spawn onto
    /// let system = EventSourceSystem::new(ps).with_codec::<DocCodec>().build();
    /// system.spawn_aggregate::<Counter>(AggregateId::new("counter")).await;
    /// # }
    /// ```
    ///
    /// The same wiring with a default store bound compiles:
    ///
    /// ```rust,no_run
    /// # use std::sync::Arc;
    /// # use bytes::Bytes;
    /// # use nitinol_eventsource::codec::Codec;
    /// # use nitinol_eventsource::system::EventSourceSystem;
    /// # use nitinol_eventsource::{Aggregate, Event};
    /// # use nitinol_persistence::store::{EventStore, InMemoryEventStore};
    /// # use nitinol_persistence::{AggregateId, EventType, Family, TypeName};
    /// # use nitinol_runtime::ProcessSystem;
    /// # #[derive(Default)] struct DocCodec;
    /// # #[derive(Clone)] struct Bumped;
    /// # impl Event for Bumped { const EVENT_TYPE: EventType = EventType::new(Family::new("doc"), TypeName::new("Bumped")); }
    /// # impl Codec<Bumped> for DocCodec {
    /// #     type Error = std::convert::Infallible;
    /// #     fn encode(_: &Bumped) -> Result<Bytes, Self::Error> { Ok(Bytes::new()) }
    /// #     fn decode(_: &[u8]) -> Result<Bumped, Self::Error> { Ok(Bumped) }
    /// # }
    /// # #[derive(Default)] struct Counter;
    /// # impl Aggregate for Counter { type Event = Bumped; fn apply(&mut self, _: Bumped) {} }
    /// # async fn wiring() {
    /// # let ps = ProcessSystem::new().await;
    /// # let store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::default());
    /// let system = EventSourceSystem::new(ps)
    ///     .with_codec::<DocCodec>()
    ///     .with_event_store(store)
    ///     .build();
    /// system.spawn_aggregate::<Counter>(AggregateId::new("counter")).await;
    /// # }
    /// ```
    pub async fn spawn_aggregate<A>(&self, id: AggregateId) -> AggregateProxy<A>
    where
        A: Aggregate,
        C: Codec<A::Event>,
    {
        self.spawn_aggregate_with_store::<A>(id, Arc::clone(self.event_store()))
            .await
    }

    /// Return an [`AggregateProps`] on the system's default [`EventStore`],
    /// pre-wired with the system event codec.
    ///
    /// Use this when additional configuration (e.g., snapshot persistor) is needed
    /// before spawning; [`aggregate_props_with_store`][Self::aggregate_props_with_store]
    /// is the same builder on a store named here instead.
    ///
    /// # Typestate
    ///
    /// Like [`spawn_aggregate`][Self::spawn_aggregate], this store-less form
    /// exists only on a system that was given a default store — the call below is
    /// a **compile error**:
    ///
    /// ```compile_fail
    /// # use std::sync::Arc;
    /// # use bytes::Bytes;
    /// # use nitinol_eventsource::codec::Codec;
    /// # use nitinol_eventsource::system::EventSourceSystem;
    /// # use nitinol_eventsource::{Aggregate, Event};
    /// # use nitinol_persistence::{AggregateId, EventType, Family, TypeName};
    /// # use nitinol_runtime::ProcessSystem;
    /// # #[derive(Default)] struct DocCodec;
    /// # #[derive(Clone)] struct Bumped;
    /// # impl Event for Bumped { const EVENT_TYPE: EventType = EventType::new(Family::new("doc"), TypeName::new("Bumped")); }
    /// # impl Codec<Bumped> for DocCodec {
    /// #     type Error = std::convert::Infallible;
    /// #     fn encode(_: &Bumped) -> Result<Bytes, Self::Error> { Ok(Bytes::new()) }
    /// #     fn decode(_: &[u8]) -> Result<Bumped, Self::Error> { Ok(Bumped) }
    /// # }
    /// # #[derive(Default)] struct Counter;
    /// # impl Aggregate for Counter { type Event = Bumped; fn apply(&mut self, _: Bumped) {} }
    /// # async fn wiring() {
    /// # let ps = ProcessSystem::new().await;
    /// // no `with_event_store` — there is no default store to build props on
    /// let system = EventSourceSystem::new(ps).with_codec::<DocCodec>().build();
    /// system.aggregate_props::<Counter>(AggregateId::new("counter"));
    /// # }
    /// ```
    pub fn aggregate_props<A>(
        &self,
        id: AggregateId,
    ) -> AggregateProps<A, crate::CodecSet<A::Event>>
    where
        A: Aggregate,
        C: Codec<A::Event>,
    {
        self.aggregate_props_with_store::<A>(id, Arc::clone(self.event_store()))
    }

    /// Spawn an aggregate process onto `store`, overriding the system default.
    ///
    /// The default receives nothing for this aggregate: `store` is the only place
    /// its stream is written and replayed.
    pub async fn spawn_aggregate_with_store<A>(
        &self,
        id: AggregateId,
        store: Arc<dyn EventStore>,
    ) -> AggregateProxy<A>
    where
        A: Aggregate,
        C: Codec<A::Event>,
    {
        self.props_on::<A>(id, store).spawn(&self.ps).await
    }

    /// Return an [`AggregateProps`] on `store`, overriding the system default.
    ///
    /// The default receives nothing for this aggregate: `store` is the only place
    /// its stream is written and replayed.
    pub fn aggregate_props_with_store<A>(
        &self,
        id: AggregateId,
        store: Arc<dyn EventStore>,
    ) -> AggregateProps<A, crate::CodecSet<A::Event>>
    where
        A: Aggregate,
        C: Codec<A::Event>,
    {
        self.props_on::<A>(id, store)
    }
}
