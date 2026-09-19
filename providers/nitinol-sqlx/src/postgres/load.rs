//! Serving a query out of the journal.
//!
//! Every condition of the query becomes part of one statement, so what a load
//! serves is one snapshot of the journal: a commit that lands while a reader is
//! being served cannot appear part-way through what it is reading.
//!
//! A row the store cannot restore is served as a failure in the stream, never
//! passed over. A reader following the global order records the highest
//! `global_sequence` it has seen and asks only for what lies beyond it, so a
//! row skipped here is a row nobody ever reads and nothing ever reports.

use bytes::Bytes;
use futures_util::StreamExt;
use jiff::Timestamp;
use nitinol_persistence::error::LoadError;
use nitinol_persistence::store::EventStream;
use nitinol_persistence::{EventRegistry, LoadQuery, LoadedEvent};
use sqlx::PgPool;

use super::failure::Failure;
use super::numbering;
use super::timestamp::Instant;

/// A stored row carries a path no collected entry answers for.
///
/// Restoring it as some other type would misroute it and skipping it would hide
/// it from every reader, so the row's load fails and the path it carried is
/// named — which is how a link that dropped a crate's declarations is told
/// apart from a corrupt row.
#[derive(Debug, thiserror::Error)]
#[error("no event type was collected under the path {path:?}")]
pub struct UnknownEventType {
    pub path: String,
}

pub(super) fn load<'a>(
    pool: &'a PgPool,
    event_types: &'a EventRegistry,
    query: LoadQuery,
) -> Result<EventStream<'a>, LoadError> {
    let conditions = Conditions::of(query)?;

    // Every condition is a parameter that may be absent, so one statement
    // answers every shape of query and the database plans it once.
    //
    // An event type prefix matches on segment boundaries: a stored path is
    // under the prefix when it is the prefix itself, or when the prefix is
    // followed by a separator. Comparing the leading characters rather than
    // matching a pattern is what keeps `_` and `%` — both legal in a family —
    // from behaving as wildcards.
    //
    // A stored path is a family and a type name with nothing after it, so the
    // second clause reaches a declaration only from a family above it. The
    // first is what a query for one declaration lands on: it has no descendant
    // of its own for the second clause to gather.
    let rows = sqlx::query_as::<_, StoredEvent>(
        r#"
        SELECT persistence_id, sequence_number, global_sequence, event_type, payload, occurred_at
          FROM nitinol_journal
         WHERE ($1::text   IS NULL OR persistence_id  =  $1)
           AND ($2::bigint IS NULL OR global_sequence >= $2)
           AND ($3::bigint IS NULL OR sequence_number >= $3)
           AND ($4::text   IS NULL OR event_type = $4
                                   OR left(event_type, length($4) + 1) = $4 || '.')
         ORDER BY global_sequence
         LIMIT $5::bigint
        "#,
    )
    .bind(conditions.stream_key)
    .bind(conditions.from_global_sequence)
    .bind(conditions.from_stream_sequence)
    .bind(conditions.event_type_prefix)
    .bind(conditions.limit)
    .fetch(pool);

    Ok(Box::pin(rows.map(move |row| {
        row.map_err(Failure::from)
            .and_then(|row| restore(event_types, row))
            .map_err(LoadError::from)
    })))
}

/// The query as the columns it is asked against.
struct Conditions {
    stream_key: Option<String>,
    from_global_sequence: Option<i64>,
    from_stream_sequence: Option<i64>,
    event_type_prefix: Option<String>,
    limit: Option<i64>,
}

impl Conditions {
    fn of(query: LoadQuery) -> Result<Self, Failure> {
        Ok(Self {
            stream_key: query.stream_key,
            from_global_sequence: query
                .from_global_sequence
                .map(|from| numbering::to_bigint("global_sequence", from))
                .transpose()?,
            from_stream_sequence: query
                .from_stream_sequence
                .map(|from| numbering::to_bigint("sequence_number", from))
                .transpose()?,
            event_type_prefix: query.event_type_prefix.map(|prefix| prefix.to_string()),
            limit: query.limit.map(i64::try_from).transpose()?,
        })
    }
}

/// A journal row as it is read, before the store's own types are put back on
/// it.
///
/// A column holding a number the store does not number with — a negative
/// sequence — fails the row rather than being folded into one the store does,
/// which would name an event other than the one stored.
#[derive(sqlx::FromRow)]
struct StoredEvent {
    persistence_id: String,
    #[sqlx(try_from = "i64")]
    sequence_number: u64,
    #[sqlx(try_from = "i64")]
    global_sequence: u64,
    event_type: String,
    payload: Vec<u8>,
    occurred_at: Instant,
}

fn restore(event_types: &EventRegistry, row: StoredEvent) -> Result<LoadedEvent, Failure> {
    let Some(event_type) = event_types.resolve(&row.event_type) else {
        return Err(UnknownEventType {
            path: row.event_type,
        }
        .into());
    };

    Ok(LoadedEvent {
        stream_key: row.persistence_id,
        sequence: row.sequence_number,
        global_sequence: row.global_sequence,
        event_type,
        payload: Bytes::from(row.payload),
        occurred_at: Timestamp::try_from(row.occurred_at)?,
    })
}
