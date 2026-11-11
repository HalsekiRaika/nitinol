//! The contract every [`Store`] owes, made executable for any assembly.
//!
//! [`Store`] states it as clauses `PRV-1` … `PRV-5`; this module is the ones a
//! suite can fix as something to run. Two of them it deliberately does not fix
//! — `PRV-2`'s connection half, which a suite cannot see, and `PRV-5`, the
//! absence of atomicity across destinations — and [`Store`] says so next to
//! each.
//!
//! # One entry point per slot, and one for the whole
//!
//! A store holds the journal and whichever further slots its backend has a
//! store for, so the entry points divide the same way the store contracts
//! already do: a store runs the ones it holds and owes the others nothing.
//! [`verify`] is the whole of them, for an assembly that holds every slot.
//!
//! Each slot entry point does three things. It fixes `PRV-1` for that slot
//! across a repeated take and across a clone of the store handle — a record
//! written through one handle and read through a second — and it puts the
//! store the slot answers with through that store's own contract, so a store
//! handing out a conforming handle is what a green run means.
//! [`verify_offset_independence`] is `PRV-4`, which belongs to neither slot
//! alone.
//!
//! # Why the clone path is a clause rather than a bound
//!
//! [`Store`] requires `Clone`, and a `Clone` that constructs fresh state
//! satisfies that bound while splitting the deployment in two. Handing a store
//! to a second wiring point *is* cloning it, so the only thing that can tell
//! the two apart is a record written through one handle and read through the
//! clone — which is what every slot entry point below does.
//!
//! # What a green run here is not
//!
//! Evidence that the deployment is wired correctly. Every clause here is one
//! the store keeps on its own: what it handed out, and what the handles it
//! handed out answered. That the framework's wiring resolves its slots from a
//! supplier rather than building stores of its own is fixed against the
//! wiring: `nitinol-eventsource/tests/system_persistence.rs` and the delivery
//! tests in `nitinol-saga` run a supplier and the processes it supplies at
//! once.

use bytes::Bytes;
use futures_core::future::BoxFuture;
use futures_util::TryStreamExt;
use jiff::Timestamp;
use nitinol_persistence::provider::{
    PersistenceProvider, ProjectionOffsetSlot, SagaOffsetSlot, SnapshotSlot, Store,
};
use nitinol_persistence::store::{Album, Journal, OffsetScope, OffsetStore};
use nitinol_persistence::{AggregateId, LoadQuery, PersistedSnapshot};

use crate::fixture::fact;
use crate::offset::fixture::framework_owned;
use crate::offset::OffsetStoreUnderTest;
use crate::snapshot::SnapshotStoreUnderTest;
use crate::under_test::{Executor, StoreUnderTest};

/// The assembly a run of the `PRV` clauses is about.
///
/// The store is an associated type rather than a `dyn` handle: the offset
/// slots answer with the backend's own store type, and what a transaction is is
/// the one thing an offset backend decides for itself.
///
/// Every clause asks for its own store, so no clause can be carried by what
/// another one left behind, and an assembly decides for itself what a store
/// holding nothing means for its backend — a fresh schema, truncated tables, a
/// pool of its own.
///
/// `Send + Sync` so that the future a clause returns is itself `Send`: an
/// assembly running the suite as an integration test may want to spawn it.
pub trait ProviderUnderTest: Send + Sync {
    /// The assembly's own store type.
    type Store: Store;

    /// Hand the suite a store holding nothing.
    fn bring_up(&self) -> BoxFuture<'_, Self::Store>;
}

/// Put a store through every clause of every slot.
///
/// This is the entry point for an assembly that holds the journal, the snapshot
/// slot and both offset slots. One that holds fewer runs the slot entry
/// points it has.
///
/// The per-slot clauses run before `PRV-4`, so a store that splits a slot's
/// world is reported on that rather than on the independence clause it then
/// cannot keep either.
pub async fn verify<U, X>(provider: &U, executor: &X)
where
    U: ProviderUnderTest,
    U::Store: SnapshotSlot + ProjectionOffsetSlot + SagaOffsetSlot,
    X: Executor,
{
    verify_event_store(provider, executor).await;
    verify_snapshot_store(provider).await;
    verify_projection_offsets(provider, executor).await;
    verify_saga_offsets(provider, executor).await;
    verify_offset_independence(provider).await;
}

/// The journal slot: `PRV-1` for it, then the whole event-store contract
/// against the handle it answers with.
pub async fn verify_event_store<U, X>(provider: &U, executor: &X)
where
    U: ProviderUnderTest,
    X: Executor,
{
    an_event_store_taken_twice_is_one_store(provider).await;
    crate::verify(&EventStoreSlot(provider), executor).await;
}

/// The snapshot slot: `PRV-1` for it, then the whole snapshot contract against
/// the handle it answers with.
///
/// No executor, because the snapshot contract has no clause that needs a writer
/// and a reader at once; the [`snapshot`](crate::snapshot) module says why.
pub async fn verify_snapshot_store<U>(provider: &U)
where
    U: ProviderUnderTest,
    U::Store: SnapshotSlot,
{
    a_snapshot_store_taken_twice_is_one_store(provider).await;
    crate::snapshot::verify(&SnapshotStoreSlot(provider)).await;
}

/// The projection-offset slot: `PRV-1` for it, then the whole offset
/// contract against the store it answers with.
pub async fn verify_projection_offsets<U, X>(provider: &U, executor: &X)
where
    U: ProviderUnderTest,
    U::Store: ProjectionOffsetSlot,
    X: Executor,
{
    const CLAUSE: &str = "PRV-1";

    let store = provider.bring_up().await;
    let supplier = PersistenceProvider::new(store.clone());
    let through_a_clone = PersistenceProvider::new(store.clone());

    let first = supplier.projection_offsets();
    let second = supplier.projection_offsets();
    let consumer = OffsetScope::new("prv.clauses.projection-slot.taken-twice");

    saved(CLAUSE, &first, &consumer, 11).await;
    assert_eq!(
        held(CLAUSE, &second, &consumer).await,
        Some(11),
        "{CLAUSE} is broken: a projection-offset slot taken twice from one store has to reach \
         one set of records, and the position saved through the first handle is not the one the \
         second answers with; a projector that re-resolved its handle would resume from nothing \
         and apply the whole journal again"
    );

    let across_the_clone = through_a_clone.projection_offsets();
    let cloned_consumer = OffsetScope::new("prv.clauses.projection-slot.across-a-clone");
    saved(CLAUSE, &first, &cloned_consumer, 19).await;
    assert_eq!(
        held(CLAUSE, &across_the_clone, &cloned_consumer).await,
        Some(19),
        "{CLAUSE} is broken: the projection slot of a clone of the store handle answers with \
         nothing the original was written with; handing a store to a second wiring point is \
         cloning it, so a `Clone` that builds fresh state leaves two projections apart with \
         nothing reported"
    );

    crate::offset::verify(&ProjectionSlotStore(provider), executor).await;
}

/// The saga-offset slot: `PRV-1` for it, then the whole offset
/// contract against the store it answers with.
pub async fn verify_saga_offsets<U, X>(provider: &U, executor: &X)
where
    U: ProviderUnderTest,
    U::Store: SagaOffsetSlot,
    X: Executor,
{
    const CLAUSE: &str = "PRV-1";

    let store = provider.bring_up().await;
    let supplier = PersistenceProvider::new(store.clone());
    let through_a_clone = PersistenceProvider::new(store.clone());

    let first = supplier.saga_offsets();
    let second = supplier.saga_offsets();
    let consumer = framework_owned("prv.clauses.saga-slot.taken-twice", 0);

    saved(CLAUSE, &first, &consumer, 13).await;
    assert_eq!(
        held(CLAUSE, &second, &consumer).await,
        Some(13),
        "{CLAUSE} is broken: a saga-offset slot taken twice from one store has to reach \
         one set of records, and the progress recorded through the first handle is not what the \
         second answers with; a host that re-resolved its handle would deliver its whole journal \
         again"
    );

    let across_the_clone = through_a_clone.saga_offsets();
    let cloned_consumer = framework_owned("prv.clauses.saga-slot.across-a-clone", 0);
    saved(CLAUSE, &first, &cloned_consumer, 23).await;
    assert_eq!(
        held(CLAUSE, &across_the_clone, &cloned_consumer).await,
        Some(23),
        "{CLAUSE} is broken: the saga slot of a clone of the store handle answers with nothing \
         the original was written with, so a host wired from the clone would deliver its whole \
         journal again"
    );

    crate::offset::verify(&SagaSlotStore(provider), executor).await;
}

/// `PRV-4`. The two offset slots are told apart: each answers for what was
/// filed through it, and a write through one leaves the other's answer where it
/// was.
///
/// The clause is about the slots rather than about the stores behind them.
/// Naming both with one handle is allowed and is how a single-table backend is
/// deployed, so what is judged here is that each consumer's own key is answered
/// — not that one slot cannot see the other's rows.
pub async fn verify_offset_independence<U>(provider: &U)
where
    U: ProviderUnderTest,
    U::Store: ProjectionOffsetSlot + SagaOffsetSlot,
{
    const CLAUSE: &str = "PRV-4";

    let supplier = PersistenceProvider::new(provider.bring_up().await);
    let projections = supplier.projection_offsets();
    let deliveries = supplier.saga_offsets();

    // The keys the two kinds of consumer really render: a user-named scope
    // verbatim, a framework-owned one inside the reserved namespace.
    let projection = OffsetScope::new("prv.clauses.independence.projection");
    let delivery = framework_owned("prv.clauses.independence.delivery", 0);

    saved(CLAUSE, &projections, &projection, 17).await;
    assert_eq!(
        held(CLAUSE, &deliveries, &delivery).await,
        None,
        "{CLAUSE} is broken: the only position saved was a projection's, under a projection's own \
         key, and the saga slot answers with one; a delivery host would resume from where a \
         projection had got to"
    );

    saved(CLAUSE, &deliveries, &delivery, 23).await;
    assert_eq!(
        held(CLAUSE, &projections, &projection).await,
        Some(17),
        "{CLAUSE} is broken: recording a delivery host's progress changed what the projection slot \
         answers with, so neither slot can be initialised or recovered without reaching the other"
    );
    assert_eq!(
        held(CLAUSE, &deliveries, &delivery).await,
        Some(23),
        "{CLAUSE} is broken: the saga slot does not answer with the progress recorded through \
         it"
    );
}

// The PRV-1 clauses whose slot has no associated store type

/// `PRV-1`, the journal slot. Handles taken twice from one store, and from a
/// clone of the store handle, reach one set of records.
async fn an_event_store_taken_twice_is_one_store<U: ProviderUnderTest>(provider: &U) {
    const CLAUSE: &str = "PRV-1";

    let store = provider.bring_up().await;
    let supplier = PersistenceProvider::new(store.clone());
    let through_a_clone = PersistenceProvider::new(store.clone());

    let first = supplier.event_store();
    let second = supplier.event_store();
    let stream = AggregateId::new("prv.clauses.event-slot.taken-twice");

    appended(CLAUSE, &first, &stream, b"written-through-the-first").await;

    assert_eq!(
        payloads(CLAUSE, &second, &stream).await,
        vec![Bytes::from_static(b"written-through-the-first")],
        "{CLAUSE} is broken: an event store taken twice from one store has to reach one set of \
         records, and what was appended through the first handle is not what the second loads; a \
         slot answering with a store of its own per call hands back two working handles onto two \
         worlds and nothing reports it"
    );

    let across_the_clone = through_a_clone.event_store();
    let cloned_stream = AggregateId::new("prv.clauses.event-slot.across-a-clone");
    appended(
        CLAUSE,
        &first,
        &cloned_stream,
        b"written-before-the-clone-read",
    )
    .await;

    assert_eq!(
        payloads(CLAUSE, &across_the_clone, &cloned_stream).await,
        vec![Bytes::from_static(b"written-before-the-clone-read")],
        "{CLAUSE} is broken: the journal of a clone of the store handle does not hold what was \
         appended through the original; handing a store to a second wiring point is cloning it, \
         so a `Clone` that builds fresh state splits the deployment in two with nothing reported"
    );
}

/// `PRV-1`, the snapshot slot.
async fn a_snapshot_store_taken_twice_is_one_store<U>(provider: &U)
where
    U: ProviderUnderTest,
    U::Store: SnapshotSlot,
{
    const CLAUSE: &str = "PRV-1";

    let store = provider.bring_up().await;
    let supplier = PersistenceProvider::new(store.clone());
    let through_a_clone = PersistenceProvider::new(store.clone());

    let first = supplier.snapshot_store();
    let second = supplier.snapshot_store();
    let aggregate = AggregateId::new("prv.clauses.snapshot-slot.taken-twice");

    saved_snapshot(CLAUSE, &first, &aggregate, 7, b"state-at-7").await;

    let found = loaded_snapshot(CLAUSE, &second, &aggregate).await;
    assert_eq!(
        found.sequence, 7,
        "{CLAUSE} is broken: the snapshot saved through the first handle was taken at sequence 7 \
         and the second answered with one at {}",
        found.sequence
    );
    assert_eq!(
        found.payload,
        Bytes::from_static(b"state-at-7"),
        "{CLAUSE} is broken: the state saved through the first handle the snapshot slot answered \
         with is not the state the second reads back"
    );

    let across_the_clone = through_a_clone.snapshot_store();
    let cloned_aggregate = AggregateId::new("prv.clauses.snapshot-slot.across-a-clone");
    saved_snapshot(CLAUSE, &first, &cloned_aggregate, 9, b"state-at-9").await;
    assert_eq!(
        loaded_snapshot(CLAUSE, &across_the_clone, &cloned_aggregate)
            .await
            .sequence,
        9,
        "{CLAUSE} is broken: the snapshot slot of a clone of the store handle does not answer with \
         what was saved through the original"
    );
}

// Staging and judging a clause

/// An append a clause requires to be taken.
async fn appended(clause: &str, store: &Journal, stream: &AggregateId, payload: &'static [u8]) {
    store
        .append(stream.as_str(), vec![fact(1, payload)])
        .await
        .unwrap_or_else(|failure| {
            panic!(
                "{clause} is broken: appending to {:?} through a handle the journal slot answered \
                 with reported {failure}",
                stream.as_str()
            )
        });
}

/// What a journal handle answers with for `stream`, in stored order.
async fn payloads(clause: &str, store: &Journal, stream: &AggregateId) -> Vec<Bytes> {
    let found: Vec<_> = store
        .load(LoadQuery::by_stream(stream))
        .await
        .unwrap_or_else(|failure| {
            panic!(
                "{clause} is broken: loading {:?} through a handle the journal slot answered with \
                 reported {failure}",
                stream.as_str()
            )
        })
        .try_collect()
        .await
        .unwrap_or_else(|failure| {
            panic!(
                "{clause} is broken: reading the stream {:?} through a handle the journal slot \
                 answered with reported {failure}",
                stream.as_str()
            )
        });

    found.into_iter().map(|event| event.payload).collect()
}

/// A snapshot save a clause requires to be taken.
async fn saved_snapshot(
    clause: &str,
    store: &Album,
    aggregate: &AggregateId,
    sequence: u64,
    payload: &'static [u8],
) {
    store
        .save(PersistedSnapshot {
            aggregate_id: aggregate.clone(),
            sequence,
            payload: Bytes::from_static(payload),
            created_at: Timestamp::now(),
        })
        .await
        .unwrap_or_else(|failure| {
            panic!(
                "{clause} is broken: saving through a handle the snapshot slot answered with \
                 reported {failure}"
            )
        });
}

/// What a snapshot handle answers with for `aggregate`.
async fn loaded_snapshot(
    clause: &str,
    store: &Album,
    aggregate: &AggregateId,
) -> PersistedSnapshot {
    store
        .load_latest(aggregate)
        .await
        .unwrap_or_else(|failure| {
            panic!(
                "{clause} is broken: loading through a handle the snapshot slot answered with \
                 reported {failure}"
            )
        })
        .unwrap_or_else(|| {
            panic!(
                "{clause} is broken: a snapshot store reached twice from one store has to reach \
                 one set of records, and a load through the second handle answered with no \
                 snapshot at all"
            )
        })
}

/// An offset save a clause requires to be taken.
///
/// Every save the suite makes is a strict move forward under a key no other
/// writer in the clause uses, which is what a correct consumer makes.
async fn saved<Cs: OffsetStore>(clause: &str, store: &Cs, scope: &OffsetScope, sequence: u64) {
    store
        .save(scope, sequence, None)
        .await
        .unwrap_or_else(|failure| {
            panic!(
                "{clause} is broken: saving the position {sequence} under {:?}, which no other \
                 writer saves under and which the slot has not been carried as far as, reported \
                 {failure}",
                scope.as_str()
            )
        });
}

/// What an offset slot answers with for `scope`.
async fn held<Cs: OffsetStore>(clause: &str, store: &Cs, scope: &OffsetScope) -> Option<u64> {
    store.load(scope).await.unwrap_or_else(|failure| {
        panic!(
            "{clause} is broken: loading the position filed under {:?} reported {failure}, and a \
             consumer cannot start without being told where it had got to",
            scope.as_str()
        )
    })
}

// Delegating a slot to the store contract it owes

/// The journal slot, as a store the event-store contract can be run against.
///
/// A store brought up per clause, exactly as the store contracts ask: every
/// clause gets a store holding nothing, and that is what an assembly's
/// `bring_up` is for.
struct EventStoreSlot<'u, U>(&'u U);

impl<U: ProviderUnderTest> StoreUnderTest for EventStoreSlot<'_, U> {
    fn bring_up(&self) -> BoxFuture<'_, Journal> {
        Box::pin(async move { PersistenceProvider::new(self.0.bring_up().await).event_store() })
    }
}

/// The snapshot slot, as a store the snapshot contract can be run against.
struct SnapshotStoreSlot<'u, U>(&'u U);

impl<U> SnapshotStoreUnderTest for SnapshotStoreSlot<'_, U>
where
    U: ProviderUnderTest,
    U::Store: SnapshotSlot,
{
    fn bring_up(&self) -> BoxFuture<'_, Album> {
        Box::pin(async move { PersistenceProvider::new(self.0.bring_up().await).snapshot_store() })
    }
}

/// The projection-offset slot, as a store the offset contract can be
/// run against.
struct ProjectionSlotStore<'u, U>(&'u U);

impl<U> OffsetStoreUnderTest for ProjectionSlotStore<'_, U>
where
    U: ProviderUnderTest,
    U::Store: ProjectionOffsetSlot,
{
    type Store = <U::Store as ProjectionOffsetSlot>::ProjectionOffsets;

    fn bring_up(&self) -> BoxFuture<'_, Self::Store> {
        Box::pin(
            async move { PersistenceProvider::new(self.0.bring_up().await).projection_offsets() },
        )
    }
}

/// The saga-offset slot, as a store the offset contract can be run
/// against.
struct SagaSlotStore<'u, U>(&'u U);

impl<U> OffsetStoreUnderTest for SagaSlotStore<'_, U>
where
    U: ProviderUnderTest,
    U::Store: SagaOffsetSlot,
{
    type Store = <U::Store as SagaOffsetSlot>::SagaOffsets;

    fn bring_up(&self) -> BoxFuture<'_, Self::Store> {
        Box::pin(async move { PersistenceProvider::new(self.0.bring_up().await).saga_offsets() })
    }
}
