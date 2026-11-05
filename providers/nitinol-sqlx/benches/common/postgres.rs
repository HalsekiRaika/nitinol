//! The Postgres a benchmark run writes in: where it comes from, the schema it
//! is given, and how that schema goes away again.
//!
//! Every bench target of this crate includes this file, so the rules below are
//! stated and kept once.
//!
//! # It runs against `DATABASE_URL` or it does not run
//!
//! There is no fallback that starts a database. A throwaway container brought
//! up per run measures that container's cold caches and its host's spare
//! capacity, which is not a baseline anything can be compared against later.
//! So with no `DATABASE_URL` a target says so on stderr and exits successfully,
//! having registered no benchmark at all.
//!
//! # The schema it writes in
//!
//! A schema of this run's own, named after the process, migrated on the way in
//! and dropped on the way out. A panic leaves it behind — the drop is on the
//! way out of a successful run and nowhere else — so a crashed run is cleaned
//! up by hand.

use std::process;

use nitinol_sqlx::PostgresEventStore;
use sqlx::postgres::PgPoolOptions;
use sqlx::{AssertSqlSafe, Executor as _, PgPool};
use tokio::runtime::Runtime;

/// Each benchmark has one call in flight at a time, so a handful is enough for
/// the pool never to be what a measurement is waiting on.
const POOL_CONNECTIONS: u32 = 4;

/// The database `DATABASE_URL` names, or `None` once stderr has been told that
/// nothing will be measured.
pub fn database_url() -> Option<String> {
    let Ok(url) = std::env::var("DATABASE_URL") else {
        eprintln!(
            "nitinol-sqlx benches: nothing to run against, so nothing was measured \
             (no DATABASE_URL)"
        );
        return None;
    };
    Some(url)
}

pub fn runtime() -> Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("a benchmark run needs a tokio runtime to drive the database on")
}

/// A migrated schema of this run's own.
pub struct BenchDatabase {
    schema: String,
    /// The connection the schema is created and dropped through, which is not
    /// the one the benchmarks run on: that one's `search_path` is already
    /// pointed at the schema.
    admin: PgPool,
    pub pool: PgPool,
}

impl BenchDatabase {
    pub async fn bring_up(url: &str) -> Self {
        let admin = PgPoolOptions::new()
            .max_connections(1)
            .connect(url)
            .await
            .expect("the database DATABASE_URL names must accept a connection");

        // A schema name cannot be a bind parameter. This one is built here out of
        // a process id, so the statement carries nothing a caller could have
        // shaped.
        let schema = format!("nitinol_sqlx_bench_{}", process::id());
        admin
            .execute(AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
            .await
            .expect("the database DATABASE_URL names must allow a schema of its own");

        let pool = pool_on(url, &schema).await;
        PostgresEventStore::migrate(&pool)
            .await
            .expect("the provider's migrations must apply to the database this run reached");

        Self {
            schema,
            admin,
            pool,
        }
    }

    /// Give the connections back and take the schema away again.
    ///
    /// Whatever store a target built over `pool` holds a handle onto it and is
    /// let go of before this is called, so the connections it reached through
    /// are given back here and not left open.
    pub async fn tear_down(self) {
        self.pool.close().await;

        self.admin
            .execute(AssertSqlSafe(format!(
                "DROP SCHEMA {} CASCADE",
                self.schema
            )))
            .await
            .expect("the schema this run created must be removable again");
        self.admin.close().await;
    }
}

async fn pool_on(url: &str, schema: &str) -> PgPool {
    let schema = schema.to_owned();
    PgPoolOptions::new()
        .max_connections(POOL_CONNECTIONS)
        .after_connect(move |connection, _| {
            let search_path = format!("SET search_path TO {schema}");
            Box::pin(async move {
                connection.execute(AssertSqlSafe(search_path)).await?;
                Ok(())
            })
        })
        .connect(url)
        .await
        .expect("the database DATABASE_URL names must accept a pool of connections")
}
