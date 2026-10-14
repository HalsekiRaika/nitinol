//! What the Postgres `OffsetStore` owes a caller, on its own.
//!
//! ```gherkin
//! Feature: nitinol-sqlx OffsetStore (Postgres)
//!
//!   Scenario: a save on its own, outside any transaction
//!     Given an offset save that is handed no transaction
//!     When save is called
//!     Then the offset is persisted
//!
//!   Scenario: a save that does not carry the offset forward is refused
//!     Given an offset a projection has already reached
//!     When a save carries that sequence again, or one behind it
//!     Then the save is refused as a conflict a caller can name
//!     And the offset is where it was
//!
//!   Scenario: two writers saving one sequence leave one winner
//!     Given two transactions open against one projection's offset
//!     When both save the same sequence
//!     Then one is taken and the other is refused as a conflict
//! ```
//!
//! The clause that puts this store under a projector — where the refusal is
//! what rolls a read-model update back — is `exactly_once_projection.rs`. What
//! is fixed here is the rule itself: which saves are taken, which are refused,
//! and that the refusal is one a caller can tell from a failure a retry would
//! clear.
//!
//! Running any of this needs a database. Where none can be reached the clause
//! judges nothing, which is not evidence that it holds.

#[path = "common/harness.rs"]
mod harness;

use std::sync::Arc;
use std::time::Duration;

use nitinol_persistence::error::OffsetError;
use nitinol_persistence::store::{OffsetScope, OffsetStore};
use nitinol_sqlx::{OffsetConflict, PostgresOffsetStore};
use sqlx::PgTransaction;
use tokio::sync::Barrier;
use tokio::task::JoinHandle;

/// How long two racing saves have to settle between them.
///
/// One of the two waits on a row the other holds until that other's
/// transaction ends, so the wait is bounded by a commit rather than by a
/// timer. The deadline is here so that a rule which never released the row
/// fails the run rather than parking it.
const RACE_DEADLINE: Duration = Duration::from_secs(30);

/// The offset the store serves for `scope`.
async fn offset(store: &PostgresOffsetStore, scope: &OffsetScope) -> Option<u64> {
    store
        .load(scope)
        .await
        .expect("a load against a database this run reached must answer")
}

/// Check that `outcome` is the refusal a save which did not carry the
/// offset forward owes its caller.
///
/// What is checked is that [`OffsetConflict`] is reachable through the
/// error, and not the words it prints: naming the type is how a caller tells a
/// projection whose single-consumer premise was broken — where a retry reaches
/// the same refusal — from a connection that dropped, where a retry is the
/// whole of the recovery.
fn refused_as_a_conflict(what: &str, outcome: Result<(), OffsetError>) {
    match outcome {
        Err(OffsetError::Backend(cause)) => assert!(
            cause.downcast_ref::<OffsetConflict>().is_some(),
            "{what} is an offset conflict, and the store reported {cause}, which a caller \
             cannot tell from a failure a retry would clear"
        ),
        Ok(()) => panic!("{what} is an offset conflict, and the store took the save"),
    }
}

#[tokio::test]
async fn a_projection_nothing_has_saved_for_has_no_offset() {
    let Some(backend) = harness::postgres_backend().await else {
        return;
    };
    let schema = backend.migrated_schema().await;
    let store = PostgresOffsetStore::new(schema.pool());
    let scope = OffsetScope::new("sqlx.offset.unsaved");

    assert_eq!(
        offset(&store, &scope).await,
        None,
        "a projection nothing has saved for has no offset, and the store answered with one"
    );
}

/// Acceptance: "a save on its own, outside any transaction".
#[tokio::test]
async fn a_save_that_was_handed_no_transaction_persists_the_offset() {
    let Some(backend) = harness::postgres_backend().await else {
        return;
    };
    let schema = backend.migrated_schema().await;
    let store = PostgresOffsetStore::new(schema.pool());
    let scope = OffsetScope::new("sqlx.offset.standalone");

    store
        .save(&scope, 5, None)
        .await
        .expect("the first save for a projection carries its offset forward");

    assert_eq!(
        offset(&store, &scope).await,
        Some(5),
        "a save handed no transaction stands on its own, and what it saved must be readable \
         afterwards"
    );
}

#[tokio::test]
async fn a_save_inside_a_transaction_lands_with_that_transaction_and_not_before() {
    let Some(backend) = harness::postgres_backend().await else {
        return;
    };
    let schema = backend.migrated_schema().await;
    let pool = schema.pool();
    let store = PostgresOffsetStore::new(pool.clone());
    let scope = OffsetScope::new("sqlx.offset.committed");

    let mut tx = pool
        .begin()
        .await
        .expect("the database this run reached must begin a transaction");
    store
        .save(&scope, 5, Some(&mut tx))
        .await
        .expect("a save inside an open transaction must be taken");

    assert_eq!(
        offset(&store, &scope).await,
        None,
        "an offset saved inside a transaction is not the projection's until that transaction \
         commits — a reader outside it must not see it yet"
    );

    tx.commit()
        .await
        .expect("a transaction the store wrote in must commit");

    assert_eq!(
        offset(&store, &scope).await,
        Some(5),
        "the commit is what makes the saved offset the projection's"
    );
}

#[tokio::test]
async fn a_save_inside_a_transaction_that_rolls_back_leaves_the_offset_where_it_was() {
    let Some(backend) = harness::postgres_backend().await else {
        return;
    };
    let schema = backend.migrated_schema().await;
    let pool = schema.pool();
    let store = PostgresOffsetStore::new(pool.clone());
    let scope = OffsetScope::new("sqlx.offset.rolled-back");

    store
        .save(&scope, 5, None)
        .await
        .expect("the first save for a projection carries its offset forward");

    let mut tx = pool
        .begin()
        .await
        .expect("the database this run reached must begin a transaction");
    store
        .save(&scope, 9, Some(&mut tx))
        .await
        .expect("a save inside an open transaction must be taken");
    tx.rollback()
        .await
        .expect("a transaction the store wrote in must roll back");

    assert_eq!(
        offset(&store, &scope).await,
        Some(5),
        "an offset saved in a transaction that rolled back never happened"
    );
}

#[tokio::test]
async fn a_sequence_the_offset_already_reached_is_refused_and_changes_nothing() {
    let Some(backend) = harness::postgres_backend().await else {
        return;
    };
    let schema = backend.migrated_schema().await;
    let store = PostgresOffsetStore::new(schema.pool());
    let scope = OffsetScope::new("sqlx.offset.duplicate");

    store
        .save(&scope, 5, None)
        .await
        .expect("the first save for a projection carries its offset forward");

    refused_as_a_conflict(
        "saving a sequence the offset already reached",
        store.save(&scope, 5, None).await,
    );

    assert_eq!(
        offset(&store, &scope).await,
        Some(5),
        "a refused save leaves the offset where it was"
    );
}

#[tokio::test]
async fn a_sequence_behind_the_offset_is_refused_and_changes_nothing() {
    let Some(backend) = harness::postgres_backend().await else {
        return;
    };
    let schema = backend.migrated_schema().await;
    let store = PostgresOffsetStore::new(schema.pool());
    let scope = OffsetScope::new("sqlx.offset.backwards");

    store
        .save(&scope, 5, None)
        .await
        .expect("the first save for a projection carries its offset forward");

    refused_as_a_conflict(
        "saving a sequence behind the offset",
        store.save(&scope, 3, None).await,
    );

    assert_eq!(
        offset(&store, &scope).await,
        Some(5),
        "a refused save leaves the offset where it was"
    );
}

/// A global subscription's offset moves over events the projection has no
/// handler for, so the sequence a save carries is not the one before it plus
/// one. The rule is that the offset moves forward, not that it moves by
/// one — a store that required the next number would refuse every projection
/// following a store-wide order.
#[tokio::test]
async fn a_sequence_past_the_offset_is_taken_even_where_it_skips_numbers() {
    let Some(backend) = harness::postgres_backend().await else {
        return;
    };
    let schema = backend.migrated_schema().await;
    let store = PostgresOffsetStore::new(schema.pool());
    let scope = OffsetScope::new("sqlx.offset.skipping");

    store
        .save(&scope, 5, None)
        .await
        .expect("the first save for a projection carries its offset forward");
    store
        .save(&scope, 9, None)
        .await
        .expect("a sequence past the offset carries it forward, whatever it skipped");

    assert_eq!(
        offset(&store, &scope).await,
        Some(9),
        "the offset must be the sequence the projection actually reached"
    );
}

/// The rule is the store's, not the caller's transaction's: a save that would
/// be refused on its own is refused inside a transaction too, which is what
/// gives the framework something to roll the read-model update back for.
#[tokio::test]
async fn a_conflict_inside_a_transaction_is_refused_the_way_one_without_a_transaction_is() {
    let Some(backend) = harness::postgres_backend().await else {
        return;
    };
    let schema = backend.migrated_schema().await;
    let pool = schema.pool();
    let store = PostgresOffsetStore::new(pool.clone());
    let scope = OffsetScope::new("sqlx.offset.duplicate-in-tx");

    store
        .save(&scope, 5, None)
        .await
        .expect("the first save for a projection carries its offset forward");

    let mut tx = pool
        .begin()
        .await
        .expect("the database this run reached must begin a transaction");
    refused_as_a_conflict(
        "saving, inside a transaction, a sequence the offset already reached",
        store.save(&scope, 5, Some(&mut tx)).await,
    );
    tx.rollback()
        .await
        .expect("a transaction the store refused must roll back");

    assert_eq!(
        offset(&store, &scope).await,
        Some(5),
        "a refused save leaves the offset where it was"
    );
}

/// One of two transactions racing to save `sequence`, and what the store
/// answered it.
///
/// Both transactions are open before either saves, and `both_ready` is what
/// makes them race rather than run one after the other: without it the first
/// task can finish outright before the second begins, which is the sequential
/// case these clauses are not about.
async fn racing_save(
    store: PostgresOffsetStore,
    scope: OffsetScope,
    sequence: u64,
    mut tx: PgTransaction<'static>,
    both_ready: Arc<Barrier>,
) -> Result<(), OffsetError> {
    both_ready.wait().await;
    match store.save(&scope, sequence, Some(&mut tx)).await {
        Ok(()) => {
            tx.commit()
                .await
                .expect("a transaction whose save the store took must commit");
            Ok(())
        }
        Err(refusal) => {
            tx.rollback()
                .await
                .expect("a transaction whose save the store refused must roll back");
            Err(refusal)
        }
    }
}

/// What the two racing saves answered, or a failure rather than a run that
/// hangs.
async fn both_settled(
    one: JoinHandle<Result<(), OffsetError>>,
    other: JoinHandle<Result<(), OffsetError>>,
) -> (Result<(), OffsetError>, Result<(), OffsetError>) {
    let settled = async {
        (
            one.await.expect("a racing save must run to completion"),
            other.await.expect("a racing save must run to completion"),
        )
    };
    tokio::time::timeout(RACE_DEADLINE, settled)
        .await
        .expect("two transactions racing for one offset must both settle")
}

fn exactly_one_winner(what: &str, one: Result<(), OffsetError>, other: Result<(), OffsetError>) {
    match (one, other) {
        (Ok(()), loser @ Err(_)) | (loser @ Err(_), Ok(())) => refused_as_a_conflict(what, loser),
        (Ok(()), Ok(())) => panic!(
            "{what} leaves one winner, and the store took both saves — a projection running \
             twice would then have applied one event twice with nothing said"
        ),
        (one, other) => panic!(
            "{what} leaves one winner, and the store refused both saves with {one:?} and {other:?}"
        ),
    }
}

/// The defence where the projection already has an offset: two writers
/// carrying it from the same value to the same next one.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_transactions_carrying_an_existing_offset_forward_leave_one_winner() {
    const WHAT: &str = "two transactions saving sequence 9 over an offset at 5";

    let Some(backend) = harness::postgres_backend().await else {
        return;
    };
    let schema = backend.migrated_schema().await;
    let pool = schema.pool();
    let store = PostgresOffsetStore::new(pool.clone());
    let scope = OffsetScope::new("sqlx.offset.race.existing");

    store
        .save(&scope, 5, None)
        .await
        .expect("the first save for a projection carries its offset forward");

    let one_tx = pool
        .begin()
        .await
        .expect("the database this run reached must begin a transaction");
    let other_tx = pool
        .begin()
        .await
        .expect("the database this run reached must begin a second transaction");
    let both_ready = Arc::new(Barrier::new(2));

    let one = tokio::spawn(racing_save(
        store.clone(),
        scope.clone(),
        9,
        one_tx,
        Arc::clone(&both_ready),
    ));
    let other = tokio::spawn(racing_save(
        store.clone(),
        scope.clone(),
        9,
        other_tx,
        both_ready,
    ));

    let (one, other) = both_settled(one, other).await;

    exactly_one_winner(WHAT, one, other);
    assert_eq!(
        offset(&store, &scope).await,
        Some(9),
        "the winner's save is the one the projection's offset carries"
    );
}

/// The same defence where the projection has no offset yet: the two
/// writers are racing to insert the row rather than to update it, and a rule
/// that only guarded the update would let both through here.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_transactions_saving_a_projection_first_offset_leave_one_winner() {
    const WHAT: &str = "two transactions saving the first offset a projection ever had";

    let Some(backend) = harness::postgres_backend().await else {
        return;
    };
    let schema = backend.migrated_schema().await;
    let pool = schema.pool();
    let store = PostgresOffsetStore::new(pool.clone());
    let scope = OffsetScope::new("sqlx.offset.race.first");

    let one_tx = pool
        .begin()
        .await
        .expect("the database this run reached must begin a transaction");
    let other_tx = pool
        .begin()
        .await
        .expect("the database this run reached must begin a second transaction");
    let both_ready = Arc::new(Barrier::new(2));

    let one = tokio::spawn(racing_save(
        store.clone(),
        scope.clone(),
        1,
        one_tx,
        Arc::clone(&both_ready),
    ));
    let other = tokio::spawn(racing_save(
        store.clone(),
        scope.clone(),
        1,
        other_tx,
        both_ready,
    ));

    let (one, other) = both_settled(one, other).await;

    exactly_one_winner(WHAT, one, other);
    assert_eq!(
        offset(&store, &scope).await,
        Some(1),
        "the winner's save is the one the projection's offset carries"
    );
}
