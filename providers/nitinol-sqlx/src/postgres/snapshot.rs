//! Where an aggregate's state at a position is kept, and which save a load
//! answers with.
//!
//! One aggregate is one row. A save is an upsert on the aggregate's key, so the
//! save that came last is the one a load answers with, and the position and the
//! state it answers with are that one save's together — a load that paired a
//! position from one save with a state from another would have the aggregate
//! restore that state and then replay from beyond the events it is missing.
//!
//! A save at a position the stored row has already reached, or behind it, is
//! taken rather than refused. The contract leaves that clause to a backend: a
//! single writer takes snapshots at positions that only grow, so taking such a
//! save and refusing it are both consistent with every consumer there is. This
//! backend takes it, which is the answer the in-memory store gives, so a
//! deployment moving between the two finds its snapshots behaving the same way
//! for it.
//!
//! Nothing here is a record. Losing a row costs a longer replay rather than a
//! fact, which is why a save has no numbering to take and no lock to hold — the
//! one row the upsert contends for is the whole of the serialisation it needs.

use async_trait::async_trait;
use bytes::Bytes;
use jiff::Timestamp;
use nitinol_persistence::error::SnapshotError;
use nitinol_persistence::store::SnapshotStore;
use nitinol_persistence::{AggregateId, PersistedSnapshot};
use sqlx::PgPool;

use super::failure::Failure;
use super::numbering;
use super::timestamp::Instant;

/// A [`SnapshotStore`] over a Postgres table.
///
/// The table is part of this crate's schema, so
/// [`PostgresEventStore::migrate`](super::PostgresEventStore::migrate) is what
/// creates it.
///
/// Cloning copies the pool handle, which reaches the same table in the same
/// database: a clone is the same data region, which is what `PRV-1` asks of a
/// store distributed by being cloned.
///
/// # The save that came last wins
///
/// `save` replaces whatever row the aggregate had, so `load_latest` answers
/// with the save that came last — and that holds for a save at a position the
/// stored snapshot has already reached or one behind it, which the contract
/// leaves to a backend. Taking it is the answer
/// [`InMemorySnapshotStore`](nitinol_persistence::store::InMemorySnapshotStore)
/// gives, so the two stores are interchangeable for a deployment that moves
/// between them.
#[derive(Clone)]
pub struct PostgresSnapshotStore {
    pool: PgPool,
}

impl PostgresSnapshotStore {
    /// A snapshot store over `pool`.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl SnapshotStore for PostgresSnapshotStore {
    async fn save(&self, snapshot: PersistedSnapshot) -> Result<(), SnapshotError> {
        replace(&self.pool, snapshot).await.map_err(Into::into)
    }

    async fn load_latest(
        &self,
        aggregate_id: &AggregateId,
    ) -> Result<Option<PersistedSnapshot>, SnapshotError> {
        stored(&self.pool, aggregate_id).await.map_err(Into::into)
    }
}

/// Make `snapshot` the row its aggregate has.
///
/// The three values one save wrote are set together, so no load can read a
/// position from this save beside a state from the one before it.
async fn replace(pool: &PgPool, snapshot: PersistedSnapshot) -> Result<(), Failure> {
    sqlx::query(
        r#"
        INSERT INTO nitinol_snapshot (aggregate_id, sequence, payload, created_at)
        VALUES ($1, $2, $3, $4)
        ON CONFLICT (aggregate_id) DO UPDATE
           SET sequence   = EXCLUDED.sequence,
               payload    = EXCLUDED.payload,
               created_at = EXCLUDED.created_at
        "#,
    )
    .bind(snapshot.aggregate_id.as_str())
    .bind(numbering::to_bigint("sequence", snapshot.sequence)?)
    .bind(snapshot.payload.as_ref())
    .bind(Instant::try_from(snapshot.created_at)?)
    .execute(pool)
    .await?;

    Ok(())
}

/// The row `aggregate_id` has, or `None` where nothing has saved for it yet.
///
/// A row this store cannot restore fails the load rather than being answered as
/// no snapshot at all: the two cannot be told apart by a caller, and reading the
/// second for the first would have an activation replay from its first event and
/// then overwrite the snapshot it could not read, with nothing said anywhere.
async fn stored(
    pool: &PgPool,
    aggregate_id: &AggregateId,
) -> Result<Option<PersistedSnapshot>, Failure> {
    let row: Option<StoredSnapshot> = sqlx::query_as(
        r#"
        SELECT sequence, payload, created_at
          FROM nitinol_snapshot
         WHERE aggregate_id = $1
        "#,
    )
    .bind(aggregate_id.as_str())
    .fetch_optional(pool)
    .await?;

    row.map(|row| restore(aggregate_id, row)).transpose()
}

/// A snapshot row as it is read, before the store's own types are put back on
/// it.
///
/// The aggregate is not among the columns: the row was asked for by its key, so
/// what the column holds is the value the caller handed over.
#[derive(sqlx::FromRow)]
struct StoredSnapshot {
    sequence: i64,
    payload: Vec<u8>,
    created_at: Instant,
}

fn restore(aggregate_id: &AggregateId, row: StoredSnapshot) -> Result<PersistedSnapshot, Failure> {
    Ok(PersistedSnapshot {
        aggregate_id: aggregate_id.clone(),
        sequence: numbering::from_bigint("sequence", row.sequence)?,
        payload: Bytes::from(row.payload),
        created_at: Timestamp::try_from(row.created_at)?,
    })
}
