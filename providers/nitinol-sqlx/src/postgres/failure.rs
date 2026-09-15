//! What a database failure on the append path means for the OCC contract.
//!
//! One place decides, so a refusal and a fault are told apart the same way
//! wherever in an append the database raised it.

use nitinol_persistence::error::AppendError;

/// Postgres' `unique_violation`.
const UNIQUE_VIOLATION: &str = "23505";

/// The index the journal's `(stream, sequence)` uniqueness is kept on.
///
/// A unique violation raised anywhere else — the global numbering's index, a
/// constraint a deployment added — is a fault in the store rather than a
/// caller's conflict, and reporting it as a conflict would hide a numbering
/// that handed one value out twice behind an error the caller is expected to
/// see.
const JOURNAL_PRIMARY_KEY: &str = "nitinol_journal_pkey";

pub(super) fn appending(key: &str, failure: sqlx::Error) -> AppendError {
    if is_sequence_conflict(&failure) {
        return AppendError::SequenceConflict(key.to_owned());
    }
    AppendError::Backend(Box::new(failure))
}

fn is_sequence_conflict(failure: &sqlx::Error) -> bool {
    let Some(database) = failure.as_database_error() else {
        return false;
    };
    database.code().as_deref() == Some(UNIQUE_VIOLATION)
        && database.constraint() == Some(JOURNAL_PRIMARY_KEY)
}
