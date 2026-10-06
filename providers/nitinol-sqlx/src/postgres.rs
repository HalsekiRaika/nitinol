mod append;
mod checkpoint;
mod failure;
mod load;
mod numbering;
mod timestamp;

use std::sync::Arc;

use async_trait::async_trait;
use nitinol_persistence::__private::linked_event_registry;
use nitinol_persistence::error::{AppendError, LoadError};
use nitinol_persistence::provider::{
    DeliveryCheckpointsProvided, Persistence, ProjectionCheckpointsProvided, Unprovided,
};
use nitinol_persistence::store::{EventStore, EventStream};
use nitinol_persistence::{
    AppendOutcome, AppendingEvent, EventRegistry, EventRegistryError, LoadQuery,
};
use sqlx::migrate::{MigrateError, Migrator};
use sqlx::PgPool;

pub use self::checkpoint::{CheckpointConflict, PostgresCheckpointStore};
pub use self::load::UnknownEventType;

/// This crate's schema — the journal and the projection checkpoints — carried
/// in the binary so a deployment applies the version of it the code was built
/// against.
static MIGRATOR: Migrator = sqlx::migrate!();

/// A [`EventStore`] over a Postgres journal.
///
/// [`EventStore`]: nitinol_persistence::store::EventStore
pub struct PostgresEventStore {
    pool: PgPool,
    event_types: &'static EventRegistry,
}

impl PostgresEventStore {
    /// A store over `pool`, serving the event types this binary linked.
    ///
    /// There is no registry to hand over, and so none to forget to hand over:
    /// what a load can restore is what
    /// [`EVENT_REGISTRY`](nitinol_persistence::EVENT_REGISTRY) collected, which
    /// is every `#[derive(Event)]` type the binary links and every record the
    /// framework journals on a saga's own stream. The store refers to the one
    /// the process built rather than building another.
    ///
    /// Opening the store is where a canonical path two declarations claim is
    /// refused, rather than at the row that would have been restored as the
    /// wrong one of them.
    pub fn new(pool: PgPool) -> Result<Self, EventRegistryError> {
        Ok(Self {
            pool,
            event_types: linked_event_registry().map_err(Clone::clone)?,
        })
    }

    /// Bring the database `pool` is connected to up to the schema this crate
    /// was built against.
    ///
    /// That schema is the crate's rather than this store's: the table
    /// [`PostgresCheckpointStore`] keeps projection checkpoints in is created
    /// here too, so a deployment running both has one migration step and not
    /// two that could be applied apart.
    pub async fn migrate(pool: &PgPool) -> Result<(), MigrateError> {
        MIGRATOR.run(pool).await
    }
}

#[async_trait]
impl EventStore for PostgresEventStore {
    async fn append(
        &self,
        key: &str,
        events: Vec<AppendingEvent>,
    ) -> Result<AppendOutcome, AppendError> {
        append::append(&self.pool, key, events).await
    }

    async fn load(&self, query: LoadQuery) -> Result<EventStream<'_>, LoadError> {
        load::load(&self.pool, self.event_types, query)
    }
}

/// The provider this backend assembles: the journal and both checkpoint slots,
/// and no snapshot store.
///
/// There is no Postgres [`SnapshotStore`] in this crate, so the snapshot slot is
/// left unprovided rather than filled from somewhere else — wiring that needs
/// snapshots is refused while it is compiled. A deployment that wants them
/// names a store for that slot itself, which is what mixing backends has to
/// look like.
///
/// [`SnapshotStore`]: nitinol_persistence::store::SnapshotStore
pub type PostgresPersistence = Persistence<
    Unprovided,
    ProjectionCheckpointsProvided<PostgresCheckpointStore>,
    DeliveryCheckpointsProvided<PostgresCheckpointStore>,
>;

/// Assemble a provider over `pool`.
///
/// Both checkpoint slots are configured with **one** store over one table,
/// which is this backend's deployment: what keeps a projection's position apart
/// from a delivery host's progress there is the key each consumer renders, not
/// a table each. The slots are still independent — neither answers for what the
/// other filed — which is the contract the provider conformance suite runs.
///
/// The journal and the checkpoints share the pool because they share the
/// database, and one migration step creates both tables. Sharing a pool is an
/// implementation of identity rather than the contract: what makes two handles
/// one world is that they reach the same rows.
///
/// Fails where two event declarations in this binary claim one canonical path,
/// which is what [`PostgresEventStore::new`] reports — the one failure that
/// belongs at assembly rather than at the row it would have restored wrongly.
pub fn postgres_persistence(pool: PgPool) -> Result<PostgresPersistence, EventRegistryError> {
    let journal: Arc<dyn EventStore> = Arc::new(PostgresEventStore::new(pool.clone())?);
    Ok(Persistence::new(journal).with_checkpoints(Arc::new(PostgresCheckpointStore::new(pool))))
}
