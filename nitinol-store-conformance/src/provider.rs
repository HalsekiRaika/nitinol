//! The contract every `PersistenceProvider` owes, made executable for any
//! assembly.
//!
//! [`PersistenceProvider`] states it as clauses `PRV-1` … `PRV-5`; this module
//! is the ones a suite can fix as something to run. Two of them it deliberately
//! does not fix — `PRV-2`'s connection half, which a suite cannot see, and
//! `PRV-5`, the absence of atomicity across destinations — and
//! [`PersistenceProvider`] says so next to each.
//!
//! # One entry point per slot, and one for the whole
//!
//! A provider supplies the journal and whichever further slots its backend has
//! a store for, so the entry points divide the same way the store contracts
//! already do: a provider runs the ones it supplies and owes the others
//! nothing. [`verify`] is the whole of them, for an assembly that supplies
//! every slot.
//!
//! Each slot entry point does two things. It fixes `PRV-1` for that slot — a
//! record written through one handle and read through a second — and it puts
//! the store the slot answers with through that store's own contract, so a
//! provider handing out a conforming handle is what a green run means.
//! [`verify_checkpoint_independence`] is `PRV-4`, which belongs to neither slot
//! alone.
//!
//! # What a green run here is not
//!
//! Evidence that the deployment is wired correctly. Every clause here is one
//! the provider keeps on its own: what it handed out, and what the stores it
//! handed out answered. That the framework's wiring resolves its slots from a
//! provider rather than building stores of its own is fixed against the wiring:
//! `nitinol-eventsource/tests/system_persistence.rs` and the delivery tests in
//! `nitinol-saga` run a provider and the processes it supplies at once.

use std::sync::Arc;

use bytes::Bytes;
use futures_core::future::BoxFuture;
use futures_util::TryStreamExt;
use jiff::Timestamp;
use nitinol_persistence::provider::{
    DeliveryCheckpointProvider, PersistenceProvider, ProjectionCheckpointProvider,
    SnapshotStoreProvider,
};
use nitinol_persistence::store::{CheckpointScope, CheckpointStore, EventStore, SnapshotStore};
use nitinol_persistence::{AggregateId, LoadQuery, PersistedSnapshot, ProjectionId};

use crate::checkpoint::fixture::framework_owned;
use crate::checkpoint::CheckpointStoreUnderTest;
use crate::fixture::fact;
use crate::snapshot::SnapshotStoreUnderTest;
use crate::under_test::{Executor, StoreUnderTest};

/// The assembly a run of the `PRV` clauses is about.
///
/// The provider is an associated type rather than a `dyn` handle: the checkpoint
/// slots answer with the backend's own store type, and what a transaction is is
/// the one thing a checkpoint backend decides for itself.
///
/// Every clause asks for its own provider, so no clause can be carried by what
/// another one left behind, and an assembly decides for itself what a provider
/// holding nothing means for its backend — a fresh schema, truncated tables, a
/// pool of its own.
///
/// `Send + Sync` so that the future a clause returns is itself `Send`: an
/// assembly running the suite as an integration test may want to spawn it.
pub trait ProviderUnderTest: Send + Sync {
    /// The assembly's own provider type.
    type Provider: PersistenceProvider;

    /// Hand the suite a provider whose stores hold nothing.
    fn bring_up(&self) -> BoxFuture<'_, Self::Provider>;
}

/// Put a provider through every clause of every slot.
///
/// This is the entry point for an assembly that supplies the journal, the
/// snapshot slot and both checkpoint slots. One that supplies fewer runs the
/// slot entry points it has.
///
/// The per-slot clauses run before `PRV-4`, so a provider that splits a slot's
/// world is reported on that rather than on the independence clause it then
/// cannot keep either.
pub async fn verify<U, X>(provider: &U, executor: &X)
where
    U: ProviderUnderTest,
    U::Provider: SnapshotStoreProvider + ProjectionCheckpointProvider + DeliveryCheckpointProvider,
    X: Executor,
{
    verify_event_store(provider, executor).await;
    verify_snapshot_store(provider).await;
    verify_projection_checkpoints(provider, executor).await;
    verify_delivery_checkpoints(provider, executor).await;
    verify_checkpoint_independence(provider).await;
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
    U::Provider: SnapshotStoreProvider,
{
    a_snapshot_store_taken_twice_is_one_store(provider).await;
    crate::snapshot::verify(&SnapshotStoreSlot(provider)).await;
}

/// The projection-checkpoint slot: `PRV-1` for it, then the whole checkpoint
/// contract against the store it answers with.
pub async fn verify_projection_checkpoints<U, X>(provider: &U, executor: &X)
where
    U: ProviderUnderTest,
    U::Provider: ProjectionCheckpointProvider,
    X: Executor,
{
    const CLAUSE: &str = "PRV-1";

    let assembled = provider.bring_up().await;
    let first = assembled.projection_checkpoints();
    let second = assembled.projection_checkpoints();
    let consumer = ProjectionId::new("prv.clauses.projection-slot.taken-twice");

    saved(CLAUSE, &first, &consumer, 11).await;
    assert_eq!(
        held(CLAUSE, &second, &consumer).await,
        Some(11),
        "{CLAUSE} is broken: a projection-checkpoint slot taken twice from one provider has to \
         reach one set of records, and the position saved through the first handle is not the one \
         the second answers with; a projector that re-resolved its handle would resume from \
         nothing and apply the whole journal again"
    );

    crate::checkpoint::verify(&ProjectionCheckpointSlot(provider), executor).await;
}

/// The delivery-checkpoint slot: `PRV-1` for it, then the whole checkpoint
/// contract against the store it answers with.
pub async fn verify_delivery_checkpoints<U, X>(provider: &U, executor: &X)
where
    U: ProviderUnderTest,
    U::Provider: DeliveryCheckpointProvider,
    X: Executor,
{
    const CLAUSE: &str = "PRV-1";

    let assembled = provider.bring_up().await;
    let first = assembled.delivery_checkpoints();
    let second = assembled.delivery_checkpoints();
    let consumer = framework_owned("prv.clauses.delivery-slot.taken-twice");

    saved(CLAUSE, &first, &consumer, 13).await;
    assert_eq!(
        held(CLAUSE, &second, &consumer).await,
        Some(13),
        "{CLAUSE} is broken: a delivery-checkpoint slot taken twice from one provider has to reach \
         one set of records, and the progress recorded through the first handle is not what the \
         second answers with; a host that re-resolved its handle would deliver its whole journal \
         again"
    );

    crate::checkpoint::verify(&DeliveryCheckpointSlot(provider), executor).await;
}

/// `PRV-4`. The two checkpoint slots are told apart: each answers for what was
/// filed through it, and a write through one leaves the other's answer where it
/// was.
///
/// The clause is about the slots rather than about the stores behind them.
/// Configuring both with one store is allowed and is how a single-table backend
/// is deployed, so what is judged here is that each consumer's own key is
/// answered — not that one slot cannot see the other's rows.
pub async fn verify_checkpoint_independence<U>(provider: &U)
where
    U: ProviderUnderTest,
    U::Provider: ProjectionCheckpointProvider + DeliveryCheckpointProvider,
{
    const CLAUSE: &str = "PRV-4";

    let assembled = provider.bring_up().await;
    let projections = assembled.projection_checkpoints();
    let deliveries = assembled.delivery_checkpoints();

    // The keys the two kinds of consumer really render: a projection verbatim,
    // a framework-owned consumer inside the reserved namespace.
    let projection = ProjectionId::new("prv.clauses.independence.projection");
    let delivery = framework_owned("prv.clauses.independence.delivery");

    saved(CLAUSE, &projections, &projection, 17).await;
    assert_eq!(
        held(CLAUSE, &deliveries, &delivery).await,
        None,
        "{CLAUSE} is broken: the only position saved was a projection's, under a projection's own \
         key, and the delivery slot answers with one; a delivery host would resume from where a \
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
        "{CLAUSE} is broken: the delivery slot does not answer with the progress recorded through \
         it"
    );
}

// The PRV-1 clauses whose slot has no associated store type

/// `PRV-1`, the journal slot. Two handles taken from one provider reach one set
/// of records.
async fn an_event_store_taken_twice_is_one_store<U: ProviderUnderTest>(provider: &U) {
    const CLAUSE: &str = "PRV-1";

    let assembled = provider.bring_up().await;
    let first = assembled.event_store();
    let second = assembled.event_store();
    let stream = AggregateId::new("prv.clauses.event-slot.taken-twice");

    first
        .append(stream.as_str(), vec![fact(1, b"written-through-the-first")])
        .await
        .unwrap_or_else(|failure| {
            panic!(
                "{CLAUSE} is broken: appending through the first handle the journal slot answered \
                 with reported {failure}"
            )
        });

    let found: Vec<_> = second
        .load(LoadQuery::by_stream(&stream))
        .await
        .unwrap_or_else(|failure| {
            panic!(
                "{CLAUSE} is broken: loading through the second handle the journal slot answered \
                 with reported {failure}"
            )
        })
        .try_collect()
        .await
        .unwrap_or_else(|failure| {
            panic!(
                "{CLAUSE} is broken: reading the stream through the second handle the journal slot \
                 answered with reported {failure}"
            )
        });

    assert_eq!(
        found
            .into_iter()
            .map(|event| event.payload)
            .collect::<Vec<_>>(),
        vec![Bytes::from_static(b"written-through-the-first")],
        "{CLAUSE} is broken: an event store taken twice from one provider has to reach one set of \
         records, and what was appended through the first handle is not what the second loads; a \
         provider answering with a store of its own per call hands back two working handles onto \
         two worlds and nothing reports it"
    );
}

/// `PRV-1`, the snapshot slot.
async fn a_snapshot_store_taken_twice_is_one_store<U>(provider: &U)
where
    U: ProviderUnderTest,
    U::Provider: SnapshotStoreProvider,
{
    const CLAUSE: &str = "PRV-1";

    let assembled = provider.bring_up().await;
    let first = assembled.snapshot_store();
    let second = assembled.snapshot_store();
    let aggregate = AggregateId::new("prv.clauses.snapshot-slot.taken-twice");

    first
        .save(PersistedSnapshot {
            aggregate_id: aggregate.clone(),
            sequence: 7,
            payload: Bytes::from_static(b"state-at-7"),
            created_at: Timestamp::now(),
        })
        .await
        .unwrap_or_else(|failure| {
            panic!(
                "{CLAUSE} is broken: saving through the first handle the snapshot slot answered \
                 with reported {failure}"
            )
        });

    let found = second
        .load_latest(&aggregate)
        .await
        .unwrap_or_else(|failure| {
            panic!(
                "{CLAUSE} is broken: loading through the second handle the snapshot slot answered \
                 with reported {failure}"
            )
        })
        .unwrap_or_else(|| {
            panic!(
                "{CLAUSE} is broken: a snapshot store taken twice from one provider has to reach \
                 one set of records, and a load through the second handle answered with no \
                 snapshot at all"
            )
        });

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
}

// Staging and judging a clause

/// A checkpoint save a clause requires to be taken.
///
/// Every save the suite makes is a strict move forward under a key no other
/// writer in the clause uses, which is what a correct consumer makes.
async fn saved<Cs: CheckpointStore>(
    clause: &str,
    store: &Arc<Cs>,
    scope: &dyn CheckpointScope,
    sequence: u64,
) {
    store
        .save(scope, sequence, None)
        .await
        .unwrap_or_else(|failure| {
            panic!(
                "{clause} is broken: saving the position {sequence} under {:?}, which no other \
                 writer saves under and which the slot has not been carried as far as, reported \
                 {failure}",
                scope.checkpoint_key()
            )
        });
}

/// What a checkpoint slot answers with for `scope`.
async fn held<Cs: CheckpointStore>(
    clause: &str,
    store: &Arc<Cs>,
    scope: &dyn CheckpointScope,
) -> Option<u64> {
    store.load(scope).await.unwrap_or_else(|failure| {
        panic!(
            "{clause} is broken: loading the position filed under {:?} reported {failure}, and a \
             consumer cannot start without being told where it had got to",
            scope.checkpoint_key()
        )
    })
}

// Delegating a slot to the store contract it owes

/// The journal slot, as a store the event-store contract can be run against.
///
/// A provider brought up per clause, exactly as the store contracts ask: every
/// clause gets a store holding nothing, and that is what an assembly's
/// `bring_up` is for.
struct EventStoreSlot<'u, U>(&'u U);

impl<U: ProviderUnderTest> StoreUnderTest for EventStoreSlot<'_, U> {
    fn bring_up(&self) -> BoxFuture<'_, Arc<dyn EventStore>> {
        Box::pin(async move { self.0.bring_up().await.event_store() })
    }
}

/// The snapshot slot, as a store the snapshot contract can be run against.
struct SnapshotStoreSlot<'u, U>(&'u U);

impl<U> SnapshotStoreUnderTest for SnapshotStoreSlot<'_, U>
where
    U: ProviderUnderTest,
    U::Provider: SnapshotStoreProvider,
{
    fn bring_up(&self) -> BoxFuture<'_, Arc<dyn SnapshotStore>> {
        Box::pin(async move { self.0.bring_up().await.snapshot_store() })
    }
}

/// The projection-checkpoint slot, as a store the checkpoint contract can be
/// run against.
struct ProjectionCheckpointSlot<'u, U>(&'u U);

impl<U> CheckpointStoreUnderTest for ProjectionCheckpointSlot<'_, U>
where
    U: ProviderUnderTest,
    U::Provider: ProjectionCheckpointProvider,
{
    type Store = <U::Provider as ProjectionCheckpointProvider>::ProjectionCheckpoints;

    fn bring_up(&self) -> BoxFuture<'_, Arc<Self::Store>> {
        Box::pin(async move { self.0.bring_up().await.projection_checkpoints() })
    }
}

/// The delivery-checkpoint slot, as a store the checkpoint contract can be run
/// against.
struct DeliveryCheckpointSlot<'u, U>(&'u U);

impl<U> CheckpointStoreUnderTest for DeliveryCheckpointSlot<'_, U>
where
    U: ProviderUnderTest,
    U::Provider: DeliveryCheckpointProvider,
{
    type Store = <U::Provider as DeliveryCheckpointProvider>::DeliveryCheckpoints;

    fn bring_up(&self) -> BoxFuture<'_, Arc<Self::Store>> {
        Box::pin(async move { self.0.bring_up().await.delivery_checkpoints() })
    }
}
