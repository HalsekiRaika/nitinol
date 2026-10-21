//! What the provider's migrations leave behind for an operator to read.
//!
//! ```gherkin
//! Feature: nitinol-sqlx offset table (Postgres)
//!
//!   Scenario: the table and its key column are named for what they hold
//!     Given a migrated schema
//!     When its tables are read
//!     Then one table named nitinol_offset holds a scope and a sequence
//!     And no column named projection_id exists
//!     And no table named nitinol_checkpoint exists
//! ```
//!
//! The rest of this provider's offset clauses are read through the store's own
//! API, which would keep them under any name. These names are not an
//! implementation detail: an operator reads the table to see where a consumer
//! has got to, moves a position by hand to let a stuck host past, and grants on
//! it. The key column holds a rendered `OffsetScope`, which names a projection,
//! a saga attachment's partition, or whatever framework-owned consumer comes
//! next — `projection_id` was the name from when a projection was the only kind
//! there was, and it says something false now.
//!
//! The old table is not migrated and not readable: there is no rename and no
//! migration tool, so a deployment creates the table afresh and its consumers
//! resume from a position someone sets.
//!
//! Running any of this needs a database. Where none can be reached the clause
//! judges nothing, which is not evidence that it holds.

#[path = "common/harness.rs"]
mod harness;

use sqlx::PgPool;

/// The columns `table_name` has in the schema `pool` is pointed at, in name
/// order — empty where the schema has no such table.
///
/// `current_schema()` rather than a name threaded in from the caller: the
/// harness points each pool at a schema of its own, so what the pool can see is
/// exactly what this clause is about.
async fn columns_of(pool: &PgPool, table_name: &str) -> Vec<String> {
    sqlx::query_scalar(
        r#"
        SELECT column_name::text
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

#[tokio::test]
async fn the_offset_table_holds_a_scope_and_a_sequence() {
    let Some(backend) = harness::postgres_backend().await else {
        return;
    };
    let schema = backend.migrated_schema().await;
    let pool = schema.pool();

    assert_eq!(
        columns_of(&pool, "nitinol_offset").await,
        vec!["scope".to_owned(), "sequence".to_owned()],
        "the migrations must leave a table named for the position it holds, \
         keyed by the rendered scope: the key is not a projection's identifier \
         any more, and an operator reading the table is told which consumer a \
         row belongs to by that column"
    );
}

#[tokio::test]
async fn no_column_is_named_for_a_projection() {
    let Some(backend) = harness::postgres_backend().await else {
        return;
    };
    let schema = backend.migrated_schema().await;
    let pool = schema.pool();

    let projection_columns: Vec<(String, String)> = sqlx::query_as(
        r#"
        SELECT table_name::text, column_name::text
          FROM information_schema.columns
         WHERE table_schema = current_schema()
           AND column_name = 'projection_id'
        "#,
    )
    .fetch_all(&pool)
    .await
    .expect("reading the catalog of a database this run reached must answer");

    assert!(
        projection_columns.is_empty(),
        "no column may be named `projection_id`: a projection is one kind of \
         consumer among several that file a position here, so a column named \
         after it would have an operator reading a saga attachment's row as a \
         projection's; found {projection_columns:?}"
    );
}

#[tokio::test]
async fn the_checkpoint_table_is_not_created_beside_the_offset_one() {
    let Some(backend) = harness::postgres_backend().await else {
        return;
    };
    let schema = backend.migrated_schema().await;
    let pool = schema.pool();

    assert!(
        columns_of(&pool, "nitinol_checkpoint").await.is_empty(),
        "the migration that created `nitinol_checkpoint` is rewritten rather \
         than followed by a rename: a deployment creates the table afresh under \
         the name it carries now, and nothing reads the old one"
    );
}
