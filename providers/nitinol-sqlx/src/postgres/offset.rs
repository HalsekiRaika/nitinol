//! Where an event consumer's progress is kept, and the rule that decides which
//! saves are taken.
//!
//! An offset moves forward or not at all. A save carrying a sequence the
//! consumer has already reached — or one behind it — is refused as
//! [`OffsetConflict`] rather than absorbed, because such a save means another
//! writer holds that key. Under `ExactlyOnce` the read-model update rides the
//! same transaction, so absorbing the save quietly is what would turn a second
//! writer into a counter incremented twice. Refusing is this backend's answer
//! to `OFS-4`, which leaves it open; a consumer does not stop on it and saves
//! again at its next move forward.
//!
//! What the rule detects is a duplicate or an offset going backwards. It is
//! not a claim that every ordering two concurrent consumers could produce is
//! caught: a sequence genuinely past the offset is taken whoever saves it.
//!
//! The comparison and the write are one statement against one row, so two
//! writers racing for one key cannot both be told they won — the second to
//! arrive waits on the row and then reads what the first left there.
//!
//! One table serves every kind of consumer. A
//! [`OffsetScope`](nitinol_persistence::store::OffsetScope) renders the
//! key, and the kinds render into namespaces that cannot meet, so there is
//! nothing a second table would keep apart.

use async_trait::async_trait;
use nitinol_persistence::error::OffsetError;
use nitinol_persistence::store::{OffsetScope, OffsetStore};
use sqlx::{PgExecutor, PgPool, PgTransaction};

use super::failure::Failure;
use super::numbering;

/// A save that would not carry a consumer's offset forward.
///
/// Published so that a caller reading into a
/// [`OffsetError::Backend`](nitinol_persistence::error::OffsetError::Backend)
/// can name what it found. A conflict says another writer holds this key. The
/// consumer does not stop: it carries on and saves again at its next move
/// forward, which is the obligation `OFS-4` states. Retrying *this* save
/// reaches the same refusal, whereas a dropped connection reported in the same
/// place is cleared by a retry — and the two therefore call for different
/// answers.
#[derive(Debug, thiserror::Error)]
#[error(
    "saving sequence {sequence} for offset key {scope:?} would not carry its offset \
     forward, so another writer has already reached it or passed it"
)]
pub struct OffsetConflict {
    scope: String,
    sequence: u64,
}

/// A [`OffsetStore`] over a Postgres table.
///
/// The table is part of this crate's schema, so
/// [`PostgresEventStore::migrate`](super::PostgresEventStore::migrate) is what
/// creates it.
///
/// Cloning copies the pool handle, which reaches the same table in the same
/// database: a clone is the same data region, which is what `PRV-1` asks of a
/// store distributed by being cloned.
#[derive(Clone)]
pub struct PostgresOffsetStore {
    pool: PgPool,
}

impl PostgresOffsetStore {
    /// An offset store over `pool`.
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
impl OffsetStore for PostgresOffsetStore {
    /// The caller's own transaction, so a read-model update and the offset
    /// that covers it land together or not at all.
    type Tx = PgTransaction<'static>;

    async fn load(&self, scope: &OffsetScope) -> Result<Option<u64>, OffsetError> {
        reached(&self.pool, scope.as_str())
            .await
            .map_err(Into::into)
    }

    async fn save(
        &self,
        scope: &OffsetScope,
        sequence: u64,
        tx: Option<&mut Self::Tx>,
    ) -> Result<(), OffsetError> {
        let key = scope.as_str();
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
async fn reached(pool: &PgPool, key: &str) -> Result<Option<u64>, Failure> {
    let saved: Option<i64> =
        sqlx::query_scalar(r#"SELECT sequence FROM nitinol_offset WHERE scope = $1"#)
            .bind(key)
            .fetch_optional(pool)
            .await?;

    Ok(saved
        .map(|sequence| numbering::from_bigint("sequence", sequence))
        .transpose()?)
}

/// Carry `key`'s offset to `sequence`, or refuse.
///
/// There is no read first and then a check: the row the insert conflicts with
/// is locked before the condition is evaluated, and a writer that finds the row
/// held waits and is then judged against the value the holder committed. Two
/// writers carrying one offset to one sequence therefore cannot both be
/// told they won.
async fn carry_forward<'e>(
    executor: impl PgExecutor<'e>,
    key: &str,
    sequence: u64,
) -> Result<(), Failure> {
    let carried = sqlx::query(
        r#"
        INSERT INTO nitinol_offset (scope, sequence)
        VALUES ($1, $2)
        ON CONFLICT (scope) DO UPDATE
           SET sequence = EXCLUDED.sequence
         WHERE nitinol_offset.sequence < EXCLUDED.sequence
        "#,
    )
    .bind(key)
    .bind(numbering::to_bigint("sequence", sequence)?)
    .execute(executor)
    .await?
    .rows_affected();

    if carried == 0 {
        return Err(OffsetConflict {
            scope: key.to_owned(),
            sequence,
        }
        .into());
    }

    Ok(())
}
