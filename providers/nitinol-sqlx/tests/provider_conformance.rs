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
//! Scenario: the two checkpoint slots are told apart inside one table
//!   Given a store whose checkpoint slots are one Postgres table
//!   When the slot-independence verification is run
//!   Then neither slot answers for what the other holds
//! ```
//!
//! This store supplies the journal and both checkpoint slots and **no snapshot
//! store**, so it runs the slot entry points it has and owes the snapshot one
//! nothing — the same division the store contracts already keep, where a
//! backend implementing one trait runs that contract and no other. The
//! whole-supplier entry point is therefore not run here; the in-memory
//! reference store is what exercises it.
//!
//! The clone clause is not a separate test: each slot entry point reads across
//! a clone of the store handle as well as across a repeated take, so the
//! Postgres handle's own `Clone` is judged wherever a slot is.
//!
//! What the independence clause is about here is worth naming: both checkpoint
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
/// the checkpoint table assembled over one pool onto it.
///
/// Every clause asks for its own, so a clause reading another's leftovers
/// cannot happen — and because the two checkpoint slots of one store are the
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

/// The projection-checkpoint slot.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_postgres_stores_projection_checkpoint_slot_conforms() {
    let Some(backend) = harness::postgres_backend().await else {
        return;
    };

    provider::verify_projection_checkpoints(&backend, &TokioExecutor).await;
}

/// The delivery-checkpoint slot.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_postgres_stores_delivery_checkpoint_slot_conforms() {
    let Some(backend) = harness::postgres_backend().await else {
        return;
    };

    provider::verify_delivery_checkpoints(&backend, &TokioExecutor).await;
}

/// The two checkpoint slots, told apart inside the one table they share.
#[tokio::test]
async fn the_postgres_stores_checkpoint_slots_are_independent() {
    let Some(backend) = harness::postgres_backend().await else {
        return;
    };

    provider::verify_checkpoint_independence(&backend).await;
}
