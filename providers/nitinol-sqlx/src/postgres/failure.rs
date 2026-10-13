//! What can go wrong inside the store, and what each failure means to a caller.
//!
//! Every fallible step of an append, a load or an offset save answers with
//! the one error below, so a step reports what went wrong without also deciding
//! what it amounts to. Reaching the contract's own errors happens here, in one
//! place per error: a refusal the caller is expected to see is told apart from
//! a fault in the store the same way wherever in an append the database raised
//! it.

use std::num::TryFromIntError;

use nitinol_persistence::error::{AppendError, LoadError, OffsetError};

use super::load::UnknownEventType;
use super::numbering::OutOfRange;
use super::offset::OffsetConflict;
use super::timestamp::InstantOutOfRange;

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

/// Anything a step of this store can fail with.
#[derive(Debug, thiserror::Error)]
pub(super) enum Failure {
    #[error(transparent)]
    Database(#[from] sqlx::Error),
    #[error(transparent)]
    Numbering(#[from] OutOfRange),
    #[error(transparent)]
    UnknownEventType(#[from] UnknownEventType),
    #[error(transparent)]
    Timestamp(#[from] InstantOutOfRange),
    /// A query counts the events it asks for in `usize`, which on a target
    /// whose pointers are wider than a `BIGINT` reaches past what the column
    /// counts in.
    #[error(transparent)]
    LimitPastBigint(#[from] TryFromIntError),
    #[error(transparent)]
    OffsetConflict(#[from] OffsetConflict),
}

impl Failure {
    /// The error the failing step raised, as it raised it.
    ///
    /// A caller that looks into a backend failure looks for a type it can name
    /// — [`UnknownEventType`] is published for exactly that — so what is handed
    /// over is that error rather than a wrapper of this crate's own it would
    /// have to unwrap first.
    fn into_cause(self) -> Box<dyn std::error::Error + Send + Sync> {
        match self {
            Self::Database(cause) => Box::new(cause),
            Self::Numbering(cause) => Box::new(cause),
            Self::UnknownEventType(cause) => Box::new(cause),
            Self::Timestamp(cause) => Box::new(cause),
            Self::LimitPastBigint(cause) => Box::new(cause),
            Self::OffsetConflict(cause) => Box::new(cause),
        }
    }
}

/// Every failure that is not a caller's conflict is a fault in the store, and a
/// load has no conflict to report at all.
impl From<Failure> for AppendError {
    fn from(failure: Failure) -> Self {
        Self::Backend(failure.into_cause())
    }
}

impl From<Failure> for LoadError {
    fn from(failure: Failure) -> Self {
        Self::Backend(failure.into_cause())
    }
}

/// `OffsetError` names no conflict of its own, so a refused save reaches
/// its caller as the backend cause `OffsetConflict` already is — the same
/// way `UnknownEventType` reaches a caller reading into a failed load.
impl From<Failure> for OffsetError {
    fn from(failure: Failure) -> Self {
        Self::Backend(failure.into_cause())
    }
}

/// What an append's failure means for the stream it was appending to.
///
/// The stream key is what makes this a decision an append owes its caller
/// rather than a plain conversion, so it stays a step of its own instead of
/// being folded into the conversions above.
pub(super) fn appending(key: &str, failure: Failure) -> AppendError {
    if matches!(&failure, Failure::Database(cause) if is_sequence_conflict(cause)) {
        return AppendError::SequenceConflict(key.to_owned());
    }
    failure.into()
}

fn is_sequence_conflict(failure: &sqlx::Error) -> bool {
    let Some(database) = failure.as_database_error() else {
        return false;
    };
    database.code().as_deref() == Some(UNIQUE_VIOLATION)
        && database.constraint() == Some(JOURNAL_PRIMARY_KEY)
}
