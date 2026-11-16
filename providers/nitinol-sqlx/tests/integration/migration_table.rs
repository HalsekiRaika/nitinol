//! Where the provider records the migrations it applied, and what that leaves
//! an application migrating the same database for itself.
//!
//! ```gherkin
//! Feature: nitinol-sqlx migration record (Postgres)
//!
//!   Scenario: the record is nitinol_migrations
//!     Given an empty schema
//!     When PostgresEventStore::migrate runs
//!     Then nitinol_migrations records versions 1 to 4 with success = true
//!     And no table named _sqlx_migrations exists
//!
//!   Scenario: an application's migrations live beside the provider's
//!     Given an application's Migrator holding a version 1, under sqlx's
//!       default table name, already applied to the same schema
//!     When PostgresEventStore::migrate runs
//!     Then it succeeds
//!     And running the application's Migrator again succeeds
//!     And the same holds with the provider migrating first and the
//!       application after
//!
//!   Scenario: migrating again is idempotent
//!     Given a migrated schema
//!     When PostgresEventStore::migrate runs again
//!     Then it succeeds and nitinol_migrations holds as many rows as before
//! ```
//!
//! sqlx keeps one record per table name, and a `Migrator` refuses a record
//! holding a version it does not carry. An application running its own
//! `sqlx::migrate!()` against the database this provider migrates therefore
//! needs the two to keep their records apart, which is what these clauses hold
//! the provider to.
//!
//! Running any of this needs a database. Where none can be reached the clause
//! judges nothing, which is not evidence that it holds.

use super::harness;

use nitinol_sqlx::PostgresEventStore;
use sqlx::migrate::{Migration, MigrationType, Migrator};
use sqlx::{PgPool, SqlSafeStr};

/// The migrations this crate carries, read from the same directory the
/// provider embeds, so the record is compared with what the crate ships rather
/// than with a copy of its version numbers.
static PROVIDER_MIGRATIONS: Migrator = sqlx::migrate!();

/// An application's own migrations: a version 1 of its own under sqlx's default
/// table name, which is the version the provider's first migration also has.
fn application_migrator() -> Migrator {
    Migrator::with_migrations(vec![Migration::new(
        1,
        "create app widget".into(),
        MigrationType::Simple,
        "CREATE TABLE app_widget (id BIGINT PRIMARY KEY)".into_sql_str(),
        false,
    )])
}

/// Whether the schema `pool` is pointed at has a table named `table_name`.
///
/// `current_schema()` rather than any schema: a database reached through
/// `DATABASE_URL` may hold a `_sqlx_migrations` of its own elsewhere, and the
/// harness points each pool at a schema only this clause uses.
async fn table_exists(pool: &PgPool, table_name: &str) -> bool {
    sqlx::query_scalar(
        r#"
        SELECT EXISTS (
            SELECT 1
              FROM information_schema.tables
             WHERE table_schema = current_schema()
               AND table_name = $1
        )
        "#,
    )
    .bind(table_name)
    .fetch_one(pool)
    .await
    .expect("reading the catalog of a database this run reached must answer")
}

async fn provider_record(pool: &PgPool) -> Vec<(i64, bool)> {
    sqlx::query_as("SELECT version, success FROM nitinol_migrations ORDER BY version")
        .fetch_all(pool)
        .await
        .expect("a schema the provider migrated must hold nitinol_migrations")
}

async fn application_record(pool: &PgPool) -> Vec<i64> {
    sqlx::query_scalar("SELECT version FROM _sqlx_migrations ORDER BY version")
        .fetch_all(pool)
        .await
        .expect("a schema the application migrated must hold _sqlx_migrations")
}

fn every_provider_migration_succeeded() -> Vec<(i64, bool)> {
    PROVIDER_MIGRATIONS
        .iter()
        .map(|migration| (migration.version, true))
        .collect()
}

#[tokio::test]
async fn the_provider_records_what_it_applied_in_nitinol_migrations() {
    let Some(backend) = harness::postgres_backend().await else {
        return;
    };
    let pool = backend.unmigrated_pool().await;

    PostgresEventStore::migrate(&pool)
        .await
        .expect("the provider's migrations must apply to an empty schema");

    assert_eq!(
        provider_record(&pool).await,
        every_provider_migration_succeeded(),
        "every migration the crate carries must be recorded as applied in \
         nitinol_migrations"
    );
}

#[tokio::test]
async fn the_provider_leaves_no_sqlx_migrations_table_behind() {
    let Some(backend) = harness::postgres_backend().await else {
        return;
    };
    let pool = backend.unmigrated_pool().await;

    PostgresEventStore::migrate(&pool)
        .await
        .expect("the provider's migrations must apply to an empty schema");

    assert!(
        !table_exists(&pool, "_sqlx_migrations").await,
        "_sqlx_migrations is the table an application's own `sqlx::migrate!()` \
         records in, so the provider must not create it"
    );
}

#[tokio::test]
async fn the_provider_migrates_a_schema_an_application_migrated_first() {
    let Some(backend) = harness::postgres_backend().await else {
        return;
    };
    let pool = backend.unmigrated_pool().await;
    let application = application_migrator();

    application
        .run(&pool)
        .await
        .expect("the application's migration must apply to an empty schema");
    PostgresEventStore::migrate(&pool).await.expect(
        "the provider must migrate a schema an application already migrated: \
         the application's version 1 is not the provider's to check",
    );
    application.run(&pool).await.expect(
        "the application's migrations must still run after the provider's: \
         its record must hold nothing it does not carry",
    );

    assert_eq!(
        application_record(&pool).await,
        vec![1],
        "_sqlx_migrations must hold the application's version alone"
    );
}

#[tokio::test]
async fn an_application_migrates_a_schema_the_provider_migrated_first() {
    let Some(backend) = harness::postgres_backend().await else {
        return;
    };
    let pool = backend.unmigrated_pool().await;
    let application = application_migrator();

    PostgresEventStore::migrate(&pool)
        .await
        .expect("the provider's migrations must apply to an empty schema");
    application.run(&pool).await.expect(
        "the application's migration must apply to a schema the provider \
         migrated: the provider's versions are not the application's to check",
    );
    PostgresEventStore::migrate(&pool)
        .await
        .expect("the provider must migrate again beside the application's record");
    application
        .run(&pool)
        .await
        .expect("the application's migrations must run again beside the provider's record");

    assert_eq!(
        application_record(&pool).await,
        vec![1],
        "_sqlx_migrations must hold the application's version alone"
    );
}

#[tokio::test]
async fn migrating_again_records_nothing_new() {
    let Some(backend) = harness::postgres_backend().await else {
        return;
    };
    let pool = backend.unmigrated_pool().await;

    PostgresEventStore::migrate(&pool)
        .await
        .expect("the provider's migrations must apply to an empty schema");
    let first = provider_record(&pool).await;
    PostgresEventStore::migrate(&pool)
        .await
        .expect("migrating a schema the provider already migrated must succeed");

    assert_eq!(
        provider_record(&pool).await.len(),
        first.len(),
        "a second migrate must add no row to nitinol_migrations"
    );
}
