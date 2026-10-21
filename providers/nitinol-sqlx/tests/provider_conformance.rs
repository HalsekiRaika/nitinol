//! The supplier contract, run against the Postgres backend assembled as one
//! store.
//!
//! ```gherkin
//! Scenario: the Postgres store keeps every clause of the slots it supplies
//!   Given a Postgres this run has reached
//!   And a store assembled from one pool over one schema
//!   When the conformance suite's PRV verification is run for each supplied slot
//!   Then every clause holds
//!
//! Scenario: the store handle's clone reaches the same rows
//!   Given a store assembled over one pool and one schema
//!   When a record is written through one handle and read through its clone
//!   Then the two are observed as one region
//!
//! Scenario: the two offset slots are told apart inside one table
//!   Given a store whose offset slots are one Postgres table
//!   When the slot-independence verification is run
//!   Then neither slot answers for what the other holds
//!
//! Scenario: the store keeps every clause of the supplier contract whole
//!   Given a Postgres this run has reached
//!   And a store assembled from one pool over one schema
//!   When the whole-supplier verification is run
//!   Then every clause holds
//! ```
//!
//! This store supplies **every** slot — the journal, the snapshot store and
//! both offset slots — so it runs each slot entry point and the whole-supplier
//! entry point besides. The two are not the same evidence. A slot entry point
//! reports a broken clause against the slot that broke it, which is what makes
//! a failure readable; the whole-supplier entry point is what a deployment
//! naming this store as its only store relies on, and running the slots one by
//! one is not a claim about the assembly they were taken from.
//!
//! The clone clause is not a separate test: each slot entry point reads across
//! a clone of the store handle as well as across a repeated take, so the
//! Postgres handle's own `Clone` is judged wherever a slot is.
//!
//! What the independence clause is about here is worth naming: both offset
//! slots are assembled over **one** store on **one** table, which is this
//! backend's deployment and the harder half of the clause. The slots are
//! independent because each consumer files under a key of its own, not because
//! each has a table — and a single pool is an implementation of identity rather
//! than the contract: what makes two handles one world is that they reach the
//! same rows, which is what the schema per `bring_up` makes observable.
//!
//! Running it needs a database. Where none can be reached the run judges
//! nothing and says so, which is not evidence of conformance.

#[path = "common/harness.rs"]
mod harness;

use futures_core::future::BoxFuture;
use harness::{PostgresBackend, TokioExecutor};
use nitinol_sqlx::PostgresStore;
use nitinol_store_conformance::provider::{self, ProviderUnderTest};

/// A store holding nothing: a schema of its own, migrated, with the journal and
/// the offset table assembled over one pool onto it.
///
/// Every clause asks for its own, so a clause reading another's leftovers
/// cannot happen — and because the two offset slots of one store are the
/// same table, a shared schema would have each clause reading the positions the
/// one before it left.
impl ProviderUnderTest for PostgresBackend {
    type Store = PostgresStore;

    fn bring_up(&self) -> BoxFuture<'_, Self::Store> {
        Box::pin(async move {
            let schema = self.migrated_schema().await;
            PostgresStore::new(schema.pool())
                .expect("the event types this test binary linked claim distinct canonical paths")
        })
    }
}

/// The journal slot: taken twice from one store, and across its clone, it is
/// one journal.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_postgres_stores_event_store_slot_conforms() {
    let Some(backend) = harness::postgres_backend().await else {
        return;
    };

    provider::verify_event_store(&backend, &TokioExecutor).await;
}

/// The snapshot slot: taken twice from one store, and across its clone, it is
/// one album — and the snapshot contract holds against the handle it answers
/// with.
///
/// No executor: the snapshot contract has no clause that needs a writer and a
/// reader at once, and a slot entry point inherits that shape rather than
/// asking for a runtime it has nothing to spawn on.
#[tokio::test]
async fn the_postgres_stores_snapshot_slot_conforms() {
    let Some(backend) = harness::postgres_backend().await else {
        return;
    };

    provider::verify_snapshot_store(&backend).await;
}

/// The projection-offset slot.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_postgres_stores_projection_offset_slot_conforms() {
    let Some(backend) = harness::postgres_backend().await else {
        return;
    };

    provider::verify_projection_offsets(&backend, &TokioExecutor).await;
}

/// The saga-offset slot.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_postgres_stores_saga_offset_slot_conforms() {
    let Some(backend) = harness::postgres_backend().await else {
        return;
    };

    provider::verify_saga_offsets(&backend, &TokioExecutor).await;
}

/// The two offset slots, told apart inside the one table they share.
#[tokio::test]
async fn the_postgres_stores_offset_slots_are_independent() {
    let Some(backend) = harness::postgres_backend().await else {
        return;
    };

    provider::verify_offset_independence(&backend).await;
}

/// Every clause of the supplier contract, against the store whole: the entry
/// point a backend supplying every slot runs.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_postgres_store_conforms_to_every_provider_clause() {
    let Some(backend) = harness::postgres_backend().await else {
        return;
    };

    provider::verify(&backend, &TokioExecutor).await;
}
