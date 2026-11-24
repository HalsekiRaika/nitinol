//! Where the journal ends.
//!
//! The head is read in one statement, so it is one snapshot of the journal:
//! the highest `global_sequence` committed when the statement started.
//!
//! That snapshot is also a complete one. Every append numbers and commits under
//! one advisory lock, in the order it was numbered in (see `append`), so no row
//! at or below a position already committed is committed afterwards. Once the
//! head is `h`, nothing at or below `h` is still to come, which is what lets a
//! reader take everything beyond `h` as what has not yet been appended.
//!
//! `global_sequence` is kept unique by `nitinol_journal_global_sequence_key`, a
//! btree the journal's migrations already create, and the highest value of a
//! btree-indexed column is read off the end of that index rather than out of
//! the table. The head therefore costs the same however long the journal
//! grows, and needs no index of its own.

use nitinol_persistence::error::LoadError;
use sqlx::PgPool;

use super::failure::Failure;
use super::numbering;

pub(super) async fn head(pool: &PgPool) -> Result<Option<u64>, LoadError> {
    read(pool).await.map_err(LoadError::from)
}

async fn read(pool: &PgPool) -> Result<Option<u64>, Failure> {
    // `MAX` over no rows is NULL, which is a journal nothing was appended to.
    let highest: Option<i64> =
        sqlx::query_scalar(r#"SELECT MAX(global_sequence) FROM nitinol_journal"#)
            .fetch_one(pool)
            .await?;

    Ok(highest
        .map(|highest| numbering::from_bigint("global_sequence", highest))
        .transpose()?)
}
