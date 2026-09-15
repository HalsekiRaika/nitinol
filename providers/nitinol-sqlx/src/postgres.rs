mod append;
mod failure;
mod load;
mod numbering;
mod timestamp;

use async_trait::async_trait;
use nitinol_persistence::error::{AppendError, LoadError};
use nitinol_persistence::store::{EventStore, EventStream};
use nitinol_persistence::{AppendOutcome, AppendingEvent, LoadQuery};
use sqlx::migrate::{MigrateError, Migrator};
use sqlx::PgPool;

use crate::event_type_registry::EventTypeRegistry;

/// The journal's schema, carried in the binary so a deployment applies the
/// version of it the code was built against.
static MIGRATOR: Migrator = sqlx::migrate!();

/// A [`EventStore`] over a Postgres journal.
///
/// [`EventStore`]: nitinol_persistence::store::EventStore
pub struct PostgresEventStore {
    pool: PgPool,
    event_types: EventTypeRegistry,
}

impl PostgresEventStore {
    /// A store over `pool`, serving the events whose types `event_types`
    /// answers for.
    pub fn new(pool: PgPool, event_types: EventTypeRegistry) -> Self {
        Self { pool, event_types }
    }

    /// Bring the database `pool` is connected to up to the schema this crate
    /// was built against.
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
        load::load(&self.pool, &self.event_types, query)
    }
}
