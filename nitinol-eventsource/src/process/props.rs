use std::marker::PhantomData;
use std::sync::Arc;

use nitinol_contract::Aggregate;
use nitinol_persistence::provider::Store;
use nitinol_persistence::AggregateId;

use crate::process::aggregate_process::AggregateProcess;
use crate::process::proxy::AggregateProxy;
use crate::process::recover::RecoverOn;
use crate::process::resolve::{Activation, AggregateResolver};
use crate::system::{EventSourceSystem, Hosted, Unhosted};

/// Which aggregate to start, and nothing else.
///
/// There is no store, no provider and no persistor here: the system a spawn
/// names is where all of them come from. What the props carry is the identity,
/// and `H` is whether that system has been named yet — [`Unhosted`] props take
/// one at [`spawn`][AggregateProps::spawn], and props obtained from
/// [`EventSourceSystem::aggregate_props`] are [`Hosted`] and spawn with nothing
/// re-injected.
///
/// Both forms meet on one private path, so the activation they reach, the
/// registry that resolves it and the journal it writes to are the same.
pub struct AggregateProps<A: Aggregate, H = Unhosted> {
    aggregate_id: AggregateId,
    host: H,
    /// `A` is named by the type alone — every ingredient an activation needs is
    /// taken from the system at the spawn — so it is carried as a marker.
    _aggregate: PhantomData<fn() -> A>,
}

impl<A: Aggregate> AggregateProps<A, Unhosted> {
    /// Configure an activation of `aggregate_id`.
    ///
    /// The identity is the whole of it. A store cannot be named here: the
    /// system this is spawned on distributes one, and reaching a second
    /// destination is a second system over the supplier that holds it.
    ///
    /// ```compile_fail
    /// # use nitinol_eventsource::AggregateProps;
    /// # use nitinol_persistence::store::InMemoryEventStore;
    /// # use nitinol_persistence::AggregateId;
    /// # struct Counter;
    /// # fn wiring() {
    /// let store = InMemoryEventStore::default();
    /// // there is no form of this that takes a journal
    /// AggregateProps::<Counter>::new(AggregateId::new("counter"), store);
    /// # }
    /// ```
    pub fn new(aggregate_id: AggregateId) -> Self {
        Self {
            aggregate_id,
            host: Unhosted,
            _aggregate: PhantomData,
        }
    }

    /// Resolve this aggregate on `system`, activating it if that system has no
    /// activation for it, and return a reference.
    ///
    /// Every ingredient comes from `system`: the journal its supplier
    /// distributes, the registry its node owns, and — for an aggregate whose
    /// [`Recover`](Aggregate::Recover) is
    /// [`Latest`](nitinol_contract::Latest) — the one snapshot persistor it
    /// hands out. This is the same resolve
    /// [`EventSourceSystem::spawn_aggregate`] performs, so two spawns of one
    /// identity on one system reach one activation rather than two writers of
    /// one stream.
    ///
    /// # Typestate
    ///
    /// A `Latest` aggregate asks its system for a snapshot store, so spawning
    /// one onto a store that holds none is a **compile error**:
    ///
    /// ```compile_fail
    /// # use bytes::Bytes;
    /// # use nitinol_eventsource::codec::Codec;
    /// # use nitinol_eventsource::system::EventSourceSystem;
    /// # use nitinol_eventsource::{Aggregate, AggregateProps, Event, Latest, Snapshotable};
    /// # use nitinol_persistence::provider::{CustomStore, PersistenceProvider};
    /// # use nitinol_persistence::store::InMemoryEventStore;
    /// # use nitinol_persistence::{AggregateId, EventType, Family, TypeName};
    /// # use nitinol_runtime::ProcessSystem;
    /// # struct DocCodec;
    /// # #[derive(Clone)] struct Counted;
    /// # impl Codec<Counted> for DocCodec {
    /// #     type Error = std::convert::Infallible;
    /// #     fn encode(_: &Counted) -> Result<Bytes, Self::Error> { Ok(Bytes::new()) }
    /// #     fn decode(_: &[u8]) -> Result<Counted, Self::Error> { Ok(Counted) }
    /// # }
    /// # impl Codec<u64> for DocCodec {
    /// #     type Error = std::convert::Infallible;
    /// #     fn encode(_: &u64) -> Result<Bytes, Self::Error> { Ok(Bytes::new()) }
    /// #     fn decode(_: &[u8]) -> Result<u64, Self::Error> { Ok(0) }
    /// # }
    /// # impl Event for Counted {
    /// #     const EVENT_TYPE: EventType = EventType::new(Family::new("doc"), TypeName::new("Counted"));
    /// #     type Codec = DocCodec;
    /// # }
    /// # #[derive(Default)] struct Counter { value: u64 }
    /// # impl Aggregate for Counter {
    /// #     type Event = Counted;
    /// #     type Recover = Latest;
    /// #     fn apply(&mut self, _: Counted) { self.value += 1; }
    /// # }
    /// # impl Snapshotable for Counter {
    /// #     type Snapshot = u64;
    /// #     type Codec = DocCodec;
    /// #     fn capture(&self) -> u64 { self.value }
    /// #     fn recover(snapshot: u64) -> Self { Self { value: snapshot } }
    /// # }
    /// # async fn wiring() {
    /// # let ps = ProcessSystem::new().await;
    /// // no `snapshot` — this store has no snapshot slot
    /// let store = CustomStore::builder().eventstore(InMemoryEventStore::default()).build();
    /// let system = EventSourceSystem::builder(ps)
    ///     .with_persistence(PersistenceProvider::new(store))
    ///     .build();
    /// AggregateProps::<Counter>::new(AggregateId::new("counter"))
    ///     .spawn(&system)
    ///     .await;
    /// # }
    /// ```
    ///
    /// Nor is a persistor wired in by hand. Which snapshot an aggregate starts
    /// from is its own type's statement, so there is no builder call to make
    /// and none to forget:
    ///
    /// ```compile_fail
    /// # use nitinol_eventsource::AggregateProps;
    /// # use nitinol_eventsource::system::EventSourceSystem;
    /// # use nitinol_persistence::provider::PersistenceProvider;
    /// # use nitinol_persistence::store::InMemoryStore;
    /// # use nitinol_persistence::AggregateId;
    /// # use nitinol_runtime::ProcessSystem;
    /// # struct Counter;
    /// # async fn wiring() {
    /// # let ps = ProcessSystem::new().await;
    /// let system = EventSourceSystem::builder(ps)
    ///     .with_persistence(PersistenceProvider::new(InMemoryStore::default()))
    ///     .build();
    /// let persistor = system.snapshot_persistor().await;
    /// // `with_snapshot_persistor` does not exist
    /// AggregateProps::<Counter>::new(AggregateId::new("counter"))
    ///     .with_snapshot_persistor(persistor);
    /// # }
    /// ```
    ///
    /// And a spawn names a system rather than a process system: resolving an
    /// identity needs a registry and a journal, which a bare
    /// [`ProcessSystem`](nitinol_runtime::ProcessSystem) has neither of:
    ///
    /// ```compile_fail
    /// # use nitinol_eventsource::AggregateProps;
    /// # use nitinol_persistence::AggregateId;
    /// # use nitinol_runtime::ProcessSystem;
    /// # struct Counter;
    /// # async fn wiring() {
    /// let ps = ProcessSystem::new().await;
    /// AggregateProps::<Counter>::new(AggregateId::new("counter"))
    ///     .spawn(&ps)
    ///     .await;
    /// # }
    /// ```
    pub async fn spawn<St>(self, system: &EventSourceSystem<St>) -> AggregateProxy<A>
    where
        St: Store,
        A::Recover: RecoverOn<A, St>,
    {
        // "Resolve *and activate*" — the caller is promised a running
        // aggregate, whether or not this call is what started it.
        let reference = reference_on::<A, St>(self.aggregate_id, system);
        reference.activate().await;
        reference
    }

    /// Carry `host` as where these props start.
    pub(crate) fn hosted<St>(self, host: EventSourceSystem<St>) -> AggregateProps<A, Hosted<St>> {
        AggregateProps {
            aggregate_id: self.aggregate_id,
            host: Hosted::new(host),
            _aggregate: PhantomData,
        }
    }
}

impl<A, St> AggregateProps<A, Hosted<St>>
where
    A: Aggregate,
    St: Store,
    A::Recover: RecoverOn<A, St>,
{
    /// Resolve this aggregate on the system these props came from, activating
    /// it if that system has no activation for it, and return a reference.
    ///
    /// Nothing is named here: the process system, the registry, the journal and
    /// the snapshot persistor are all the ones the props were taken from. The
    /// identity they carry is used only if this call is the one that activates
    /// the aggregate, because a second activation of one stream is what the
    /// resolve layer exists to prevent.
    pub async fn spawn(self) -> AggregateProxy<A> {
        let reference = self.into_reference();
        reference.activate().await;
        reference
    }

    /// A reference that resolves this aggregate at dispatch time, activating
    /// nothing yet.
    ///
    /// The synchronous half of the resolve entry points: a caller that cannot
    /// await — a `fold` over a decision, or a saga's delivery addressing a
    /// target it has not yet activated — still gets a reference addressing the
    /// same aggregate every other entry point does.
    pub(crate) fn into_reference(self) -> AggregateProxy<A> {
        reference_on::<A, St>(self.aggregate_id, self.host.system())
    }
}

/// The one road both entry points take.
///
/// Every ingredient an activation runs on is taken from `system` here and
/// nowhere else, so the two spawn forms cannot drift into reaching different
/// stores, different registries or different persistors.
fn reference_on<A, St>(
    aggregate_id: AggregateId,
    system: &EventSourceSystem<St>,
) -> AggregateProxy<A>
where
    A: Aggregate,
    St: Store,
    A::Recover: RecoverOn<A, St>,
{
    // The handle carries the system a later re-resolve spawns on, so the
    // activation this reference may start and the one that replaces it are
    // identical.
    let handle = system.resolve_handle();
    let store = system.persistence().event_store();
    let recovery = <A::Recover as RecoverOn<A, St>>::recovery(system);

    let identity = aggregate_id.clone();
    let activation: Activation<A> = Arc::new(move || AggregateProcess {
        state: A::default(),
        aggregate_id: identity.clone(),
        store: store.clone(),
        sequence: 0,
        recovery: recovery.clone(),
    });

    let resolver = AggregateResolver::new(handle, aggregate_id.clone(), activation);
    AggregateProxy::resolved(aggregate_id, resolver)
}
