//! What the provider's migrations leave behind for a snapshot to be kept in.
//!
//! ```gherkin
//! Feature: nitinol-sqlx snapshot table (Postgres)
//!
//!   Scenario: one aggregate is one row
//!     Given a migrated schema
//!     When its tables are read
//!     Then one table named nitinol_snapshot holds an aggregate_id, a
//!          sequence, a payload and a created_at, none of them nullable
//!     And the aggregate_id alone is its primary key
//! ```
//!
//! The rest of this provider's snapshot clauses are read through the store's
//! own API, which would keep them whatever the table held. Two things it cannot
//! see are fixed here instead.
//!
//! The first is that an aggregate has **one** row rather than a history of
//! them. A store that inserted a row per save and served the highest sequence
//! back would answer every read the store's own API makes the same way, right
//! up to a save behind the stored position — and it would grow without bound
//! for a shortcut that is never a record. The key is what makes a save replace
//! rather than accumulate, so the key is what is read.
//!
//! The second is that the four values a snapshot is made of are all required.
//! A nullable column would let a row exist that no save ever wrote whole, and
//! a state and a position that did not belong together is the one way a
//! snapshot loses events with no error raised anywhere.
//!
//! Running any of this needs a database. Where none can be reached the clause
//! judges nothing, which is not evidence that it holds.

#[path = "common/harness.rs"]
mod harness;

use sqlx::PgPool;

/// Every column `table_name` has in the schema `pool` is pointed at, as
/// `(name, type, whether it may be null)` in name order — empty where the
/// schema has no such table.
///
/// `current_schema()` rather than a name threaded in from the caller: the
/// harness points each pool at a schema of its own, so what the pool can see is
/// exactly what this clause is about.
async fn columns_of(pool: &PgPool, table_name: &str) -> Vec<(String, String, String)> {
    sqlx::query_as(
        r#"
        SELECT column_name::text, data_type::text, is_nullable::text
          FROM information_schema.columns
         WHERE table_schema = current_schema()
           AND table_name = $1
         ORDER BY column_name
        "#,
    )
    .bind(table_name)
    .fetch_all(pool)
    .await
    .expect("reading the catalog of a database this run reached must answer")
}

/// The columns `table_name`'s primary key is made of, in key order — empty
/// where it has no primary key, or no such table exists.
async fn primary_key_of(pool: &PgPool, table_name: &str) -> Vec<String> {
    sqlx::query_scalar(
        r#"
        SELECT keyed.column_name::text
          FROM information_schema.table_constraints AS constraint_of
          JOIN information_schema.key_column_usage AS keyed
            ON keyed.constraint_name = constraint_of.constraint_name
           AND keyed.constraint_schema = constraint_of.constraint_schema
         WHERE constraint_of.table_schema = current_schema()
           AND constraint_of.table_name = $1
           AND constraint_of.constraint_type = 'PRIMARY KEY'
         ORDER BY keyed.ordinal_position
        "#,
    )
    .bind(table_name)
    .fetch_all(pool)
    .await
    .expect("reading the catalog of a database this run reached must answer")
}

fn required(name: &str, data_type: &str) -> (String, String, String) {
    (name.to_owned(), data_type.to_owned(), "NO".to_owned())
}

#[tokio::test]
async fn the_snapshot_table_holds_a_state_and_the_position_it_belongs_to() {
    let Some(backend) = harness::postgres_backend().await else {
        return;
    };
    let schema = backend.migrated_schema().await;
    let pool = schema.pool();

    assert_eq!(
        columns_of(&pool, "nitinol_snapshot").await,
        vec![
            required("aggregate_id", "text"),
            required("created_at", "timestamp with time zone"),
            required("payload", "bytea"),
            required("sequence", "bigint"),
        ],
        "the migrations must leave a table a snapshot is kept whole in: the aggregate it is \
         for, the position it was taken at, the state at that position, and the instant the \
         caller took it — every one of them required, because a row missing any of the four \
         is a snapshot no save ever wrote"
    );
}

#[tokio::test]
async fn the_snapshot_table_is_keyed_by_the_aggregate_alone() {
    let Some(backend) = harness::postgres_backend().await else {
        return;
    };
    let schema = backend.migrated_schema().await;
    let pool = schema.pool();

    assert_eq!(
        primary_key_of(&pool, "nitinol_snapshot").await,
        vec!["aggregate_id".to_owned()],
        "an aggregate has one snapshot row and not a history of them, and the key is what makes \
         a save replace the one before it rather than accumulate beside it; a key carrying the \
         sequence as well would keep every snapshot ever taken of a shortcut that is never a \
         record"
    );
}
