//! What a run of the provider's contract tests needs: a Postgres to run
//! against, and stores over it that hold nothing.
//!
//! A database is reached in one of three ways, decided once here rather than
//! in each test:
//!
//! 1. `DATABASE_URL` names one, and that one is used.
//! 2. There is no `DATABASE_URL`, but a container runtime is reachable: the
//!    test that asks starts a Postgres of its own, and dropping the value it
//!    was handed removes that container again — on the way out of the test and
//!    on the way out of a panic alike.
//! 3. Neither: nothing is reachable, and the test returns without judging the
//!    provider.
//!
//! Only the third leaves a test passing on no evidence, and it is the only one
//! that may. Once a database has been reached, failing to connect to it, to
//! migrate it, or to keep a clause is a test failure — never a quiet return.
//!
//! Each test binary compiles this module in full but uses only the subset it
//! needs, so per-binary dead code is expected.

#![allow(dead_code)]

use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use futures_core::future::BoxFuture;
use nitinol_persistence::store::EventStore;
use nitinol_persistence::{EventType, Family, TypeName};
use nitinol_sqlx::PostgresEventStore;
use nitinol_store_conformance::{Executor, StoreUnderTest};
use sqlx::postgres::PgPoolOptions;
use sqlx::{AssertSqlSafe, Executor as _, PgPool};
use testcontainers::core::WaitFor;
use testcontainers::runners::AsyncRunner;
use testcontainers::{ContainerAsync, Healthcheck, ImageExt};
use testcontainers_modules::postgres::Postgres;

/// The one event type the conformance suite appends.
///
/// The suite keeps its own constant to itself and its reader panics on a load
/// it cannot decode, so a store brought up without this type registered fails
/// every clause that reads anything back.
pub const CONFORMANCE_FACT: EventType =
    EventType::new(Family::new("occ.conformance"), TypeName::new("Fact"));

/// The image a run starts for itself, pinned by digest.
///
/// A tag moves. A database version that changes underneath the suite would
/// change what a green run means with nothing in the repository having changed.
const POSTGRES_IMAGE_TAG: &str =
    "17-alpine@sha256:18cfe3ef5e6815560c98237d6216d1e5119702fb0f3894c8785dd58b8bbe5d73";

/// Postgres accepts connections on TCP only once the real server is up: the
/// temporary one the official image runs while it initialises listens on a unix
/// socket alone, and its log line is otherwise indistinguishable from the real
/// one's.
const READY_PROBE: [&str; 7] = [
    "pg_isready",
    "-h",
    "localhost",
    "-p",
    "5432",
    "-U",
    "postgres",
];

/// Four concurrent writers and a reader is what the conformance suite's
/// concurrency clause runs, and a writer queued behind another's connection
/// makes that clause a sequential run wearing a spawn.
const POOL_CONNECTIONS: u32 = 8;

/// A store holding nothing, and the schema it was given.
///
/// The schema is handed back so a second store can be opened over the same rows
/// — the only way to read what one store wrote through a registry that does not
/// know the event type it carries.
pub struct EmptyStore {
    pub schema: String,
    pub store: Arc<dyn EventStore>,
}

/// The Postgres a test run reaches, and the stores it brings up over it.
pub struct PostgresBackend {
    /// Owned for as long as the test that asked for it: dropping it removes the
    /// container this run started.
    _container: Option<ContainerAsync<Postgres>>,
    url: String,
    admin: PgPool,
    event_types: Vec<EventType>,
}

/// How many schemas this test binary has handed out.
///
/// Process-wide rather than per-backend: `cargo test` runs the test functions
/// of one binary in parallel threads of one process, and every one of them asks
/// for a backend of its own, so a counter each would have them all start again
/// at the same name.
static SCHEMAS: AtomicU64 = AtomicU64::new(0);

/// The Postgres this run has, or `None` when the environment offers none.
///
/// `event_types` is what every store brought up here resolves as it loads: a
/// row carrying anything else fails its load rather than being passed over.
pub async fn postgres_backend(event_types: &[EventType]) -> Option<PostgresBackend> {
    let (container, url) = match std::env::var("DATABASE_URL") {
        Ok(url) => (None, url),
        Err(_) => match container_runtime_reach() {
            Ok(()) => {
                let container = start_postgres().await;
                let url = connection_url(&container).await;
                (Some(container), url)
            }
            Err(reason) => {
                eprintln!(
                    "nitinol-sqlx contract tests: nothing to run against, so nothing was judged \
                     (no DATABASE_URL, and {reason})"
                );
                return None;
            }
        },
    };

    let admin = PgPoolOptions::new()
        .max_connections(1)
        .connect(&url)
        .await
        .expect("the database this run reached must accept a connection");

    Some(PostgresBackend {
        _container: container,
        url,
        admin,
        event_types: event_types.to_vec(),
    })
}

impl PostgresBackend {
    /// A store holding nothing: a schema of its own, migrated, over a pool of
    /// its own.
    pub async fn bring_up_store(&self) -> EmptyStore {
        let schema = self.create_schema().await;
        let store = self.store_on(&schema, &self.event_types).await;
        EmptyStore { schema, store }
    }

    /// Another store over the rows in `schema`, resolving `event_types` and
    /// nothing besides.
    pub async fn store_on(&self, schema: &str, event_types: &[EventType]) -> Arc<dyn EventStore> {
        let pool = self.pool_on(schema).await;
        PostgresEventStore::migrate(&pool)
            .await
            .expect("the provider's migrations must apply to a database it has reached");
        let registry = event_types.iter().copied().collect();
        Arc::new(PostgresEventStore::new(pool, registry)) as Arc<dyn EventStore>
    }

    /// A schema no other store in this run writes to.
    ///
    /// `cargo test` runs test functions in parallel and the conformance suite
    /// asks for a store holding nothing seven times over, so a shared table
    /// would have each of them reading another's rows.
    async fn create_schema(&self) -> String {
        let schema = format!(
            "nitinol_sqlx_{}_{}",
            process::id(),
            SCHEMAS.fetch_add(1, Ordering::SeqCst)
        );
        // A schema name cannot be a bind parameter. This one is built here out
        // of a process id and a counter, so the statement carries nothing a
        // caller could have shaped.
        self.admin
            .execute(AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
            .await
            .expect("the database this run reached must allow a schema of its own");
        schema
    }

    async fn pool_on(&self, schema: &str) -> PgPool {
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
            .connect(&self.url)
            .await
            .expect("the database this run reached must accept a pool of connections")
    }
}

impl StoreUnderTest for PostgresBackend {
    fn bring_up(&self) -> BoxFuture<'_, Arc<dyn EventStore>> {
        Box::pin(async move { self.bring_up_store().await.store })
    }
}

/// The caller's runtime, lent to the conformance suite for the clause that
/// needs writers making progress while a reader reads.
pub struct TokioExecutor;

impl Executor for TokioExecutor {
    fn spawn(&self, task: BoxFuture<'static, ()>) -> BoxFuture<'static, ()> {
        let handle = tokio::spawn(task);
        Box::pin(async move {
            handle
                .await
                .expect("a task the suite spawned must run to completion");
        })
    }

    fn yield_now(&self) -> BoxFuture<'static, ()> {
        Box::pin(tokio::task::yield_now())
    }
}

async fn start_postgres() -> ContainerAsync<Postgres> {
    Postgres::default()
        .with_tag(POSTGRES_IMAGE_TAG)
        .with_health_check(
            Healthcheck::cmd(READY_PROBE)
                .with_interval(Duration::from_millis(250))
                .with_timeout(Duration::from_secs(5))
                .with_retries(240),
        )
        .with_ready_conditions(vec![WaitFor::healthcheck()])
        .start()
        .await
        .expect("a container runtime this run reached must start the pinned Postgres image")
}

async fn connection_url(container: &ContainerAsync<Postgres>) -> String {
    let host = container
        .get_host()
        .await
        .expect("a started container must report the host it is reachable at");
    let port = container
        .get_host_port_ipv4(5432)
        .await
        .expect("a started Postgres container must report its mapped port");
    format!("postgres://postgres:postgres@{host}:{port}/postgres")
}

/// Whether a container runtime is reachable, and what stood in the way when it
/// is not.
///
/// Reaching one is what separates a run that starts its own database from a run
/// that has nothing to judge, so the answer is a connection that either
/// succeeds or does not, rather than a socket file that happens to exist.
fn container_runtime_reach() -> Result<(), String> {
    let socket = match std::env::var("DOCKER_HOST") {
        // A host configured as tcp:// or ssh:// is taken at its word. Nothing
        // here can tell "not configured" from "configured and down", and
        // reading the second as the first would turn a broken environment into
        // a run that judged nothing and said it was fine.
        Ok(host) => match host.strip_prefix("unix://") {
            Some(path) => PathBuf::from(path),
            None => return Ok(()),
        },
        Err(_) => default_socket().ok_or_else(|| {
            "no container runtime socket exists at any of the usual locations".to_owned()
        })?,
    };

    connects(&socket)
}

/// Where a container runtime's socket is looked for, in the order the runner
/// itself looks.
fn default_socket() -> Option<PathBuf> {
    let mut candidates = vec![PathBuf::from("/var/run/docker.sock")];
    if let Ok(runtime_dir) = std::env::var("XDG_RUNTIME_DIR") {
        candidates.push(Path::new(&runtime_dir).join(".docker/run/docker.sock"));
    }
    if let Ok(home) = std::env::var("HOME") {
        candidates.push(Path::new(&home).join(".docker/run/docker.sock"));
        candidates.push(Path::new(&home).join(".docker/desktop/docker.sock"));
    }
    candidates.into_iter().find(|candidate| candidate.exists())
}

fn connects(socket: &Path) -> Result<(), String> {
    UnixStream::connect(socket).map(|_| ()).map_err(|failure| {
        format!(
            "the container runtime socket at {} refused a connection: {failure}",
            socket.display()
        )
    })
}
