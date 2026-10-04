//! Where an event consumer's progress is kept, and the rule that decides which
//! saves are taken.
//!
//! A checkpoint moves forward or not at all. A save carrying a sequence the
//! consumer has already reached — or one behind it — is refused as
//! [`CheckpointConflict`] rather than absorbed, because the only way a
//! consumer is asked to cover a sequence twice is that something other than
//! its one owner has been writing to it. Under `ExactlyOnce` the read-model
//! update rides the same transaction, so absorbing the save quietly is what
//! would turn that broken premise into a counter incremented twice.
//!
//! What the rule detects is a duplicate or a checkpoint going backwards. It is
//! not a claim that every ordering two concurrent consumers could produce is
//! caught: a sequence genuinely past the checkpoint is taken whoever saves it.
//!
//! The comparison and the write are one statement against one row, so two
//! writers racing for one key cannot both be told they won — the second to
//! arrive waits on the row and then reads what the first left there.
//!
//! One table serves every kind of consumer. A
//! [`CheckpointScope`](nitinol_persistence::store::CheckpointScope) renders the
//! key, and the kinds render into namespaces that cannot meet, so there is
//! nothing a second table would keep apart.

use async_trait::async_trait;
use nitinol_persistence::error::CheckpointError;
use nitinol_persistence::store::{CheckpointScope, CheckpointStore};
use sqlx::{PgExecutor, PgPool, PgTransaction};

use super::failure::Failure;
use super::numbering;

/// A save that would not carry a consumer's checkpoint forward.
///
/// Published so that a caller reading into a
/// [`CheckpointError::Backend`](nitinol_persistence::error::CheckpointError::Backend)
/// can name what it found. A conflict says a premise was broken — a second
/// writer on a key that is meant to have one owner — and retrying reaches the
/// same refusal. A dropped connection reported in the same place is cleared by
/// a retry, and the two call for different answers.
#[derive(Debug, thiserror::Error)]
#[error(
    "saving sequence {sequence} for checkpoint key {scope:?} would not carry its checkpoint \
     forward, so another writer has already reached it or passed it"
)]
pub struct CheckpointConflict {
    scope: String,
    sequence: u64,
}

/// A [`CheckpointStore`] over a Postgres table.
///
/// The table is part of this crate's schema, so
/// [`PostgresEventStore::migrate`](super::PostgresEventStore::migrate) is what
/// creates it.
pub struct PostgresCheckpointStore {
    pool: PgPool,
}

impl PostgresCheckpointStore {
    /// A checkpoint store over `pool`.
    ///
    /// A save that is handed no transaction of the caller's runs on this pool.
    /// One that is handed a transaction runs inside it and is the caller's to
    /// commit, so the pool that transaction came from is the caller's decision
    /// rather than this one.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl CheckpointStore for PostgresCheckpointStore {
    /// The caller's own transaction, so a read-model update and the checkpoint
    /// that covers it land together or not at all.
    type Tx = PgTransaction<'static>;

    async fn load(&self, scope: &dyn CheckpointScope) -> Result<Option<u64>, CheckpointError> {
        reached(&self.pool, scope.checkpoint_key())
            .await
            .map_err(Into::into)
    }

    async fn save(
        &self,
        scope: &dyn CheckpointScope,
        sequence: u64,
        tx: Option<&mut Self::Tx>,
    ) -> Result<(), CheckpointError> {
        let key = scope.checkpoint_key();
        // The rule is the same either way: a save the store would refuse on its
        // own is refused inside a caller's transaction too, which is what gives
        // `ExactlyOnce` delivery something to roll the read-model update back
        // for.
        match tx {
            Some(tx) => carry_forward(&mut **tx, key, sequence).await,
            None => carry_forward(&self.pool, key, sequence).await,
        }
        .map_err(Into::into)
    }
}

/// The sequence `key` has been carried as far as, or `None` where nothing has
/// saved for it yet.
///
/// The column is still named for the one kind of consumer that existed when it
/// was created; what it holds is a rendered
/// [`CheckpointScope`](nitinol_persistence::store::CheckpointScope), and a
/// projection renders verbatim, so every row written before is read back
/// unchanged.
async fn reached(pool: &PgPool, key: &str) -> Result<Option<u64>, Failure> {
    let saved: Option<i64> =
        sqlx::query_scalar(r#"SELECT sequence FROM nitinol_checkpoint WHERE projection_id = $1"#)
            .bind(key)
            .fetch_optional(pool)
            .await?;

    Ok(saved
        .map(|sequence| numbering::from_bigint("sequence", sequence))
        .transpose()?)
}

/// Carry `key`'s checkpoint to `sequence`, or refuse.
///
/// There is no read first and then a check: the row the insert conflicts with
/// is locked before the condition is evaluated, and a writer that finds the row
/// held waits and is then judged against the value the holder committed. Two
/// writers carrying one checkpoint to one sequence therefore cannot both be
/// told they won.
async fn carry_forward<'e>(
    executor: impl PgExecutor<'e>,
    key: &str,
    sequence: u64,
) -> Result<(), Failure> {
    let carried = sqlx::query(
        r#"
        INSERT INTO nitinol_checkpoint (projection_id, sequence)
        VALUES ($1, $2)
        ON CONFLICT (projection_id) DO UPDATE
           SET sequence = EXCLUDED.sequence
         WHERE nitinol_checkpoint.sequence < EXCLUDED.sequence
        "#,
    )
    .bind(key)
    .bind(numbering::to_bigint("sequence", sequence)?)
    .execute(executor)
    .await?
    .rows_affected();

    if carried == 0 {
        return Err(CheckpointConflict {
            scope: key.to_owned(),
            sequence,
        }
        .into());
    }

    Ok(())
}
