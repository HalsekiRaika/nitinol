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
use nitinol_persistence::error::LoadError;
use nitinol_persistence::store::EventStream;
use nitinol_persistence::{LoadQuery, LoadedEvent};
use sqlx::postgres::PgRow;
use sqlx::{PgPool, Row};
use time::OffsetDateTime;

use super::{numbering, timestamp};
use crate::event_type_registry::{EventTypeRegistry, UnknownEventType};

/// Every condition is a parameter that may be absent, so one statement answers
/// every shape of query and the database plans it once.
///
/// An event type prefix matches on segment boundaries: a stored path is under
/// the prefix when it is the prefix itself, or when the prefix is followed by a
/// separator. Comparing the leading characters rather than matching a pattern
/// is what keeps `_` and `%` — both legal in a family — from behaving as
/// wildcards.
const SERVE_QUERY: &str = "\
SELECT persistence_id, sequence_number, global_sequence, event_type, payload, occurred_at \
  FROM nitinol_journal \
 WHERE ($1::text   IS NULL OR persistence_id  =  $1) \
   AND ($2::bigint IS NULL OR global_sequence >= $2) \
   AND ($3::bigint IS NULL OR sequence_number >= $3) \
   AND ($4::text   IS NULL OR event_type = $4 \
                           OR left(event_type, length($4) + 1) = $4 || '.') \
 ORDER BY global_sequence \
 LIMIT $5::bigint";

pub(super) fn load<'a>(
    pool: &'a PgPool,
    event_types: &'a EventTypeRegistry,
    query: LoadQuery,
) -> Result<EventStream<'a>, LoadError> {
    let conditions = Conditions::of(query)?;

    let rows = sqlx::query(SERVE_QUERY)
        .bind(conditions.stream_key)
        .bind(conditions.from_global_sequence)
        .bind(conditions.from_stream_sequence)
        .bind(conditions.event_type_prefix)
        .bind(conditions.limit)
        .fetch(pool);

    Ok(Box::pin(rows.map(move |row| {
        restore(event_types, &row.map_err(backend)?)
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
    fn of(query: LoadQuery) -> Result<Self, LoadError> {
        Ok(Self {
            stream_key: query.stream_key,
            from_global_sequence: query
                .from_global_sequence
                .map(|from| numbering::to_bigint("global_sequence", from))
                .transpose()
                .map_err(backend)?,
            from_stream_sequence: query
                .from_stream_sequence
                .map(|from| numbering::to_bigint("sequence_number", from))
                .transpose()
                .map_err(backend)?,
            event_type_prefix: query.event_type_prefix.map(|prefix| prefix.to_string()),
            limit: query
                .limit
                .map(|limit| i64::try_from(limit).map_err(backend))
                .transpose()?,
        })
    }
}

fn restore(event_types: &EventTypeRegistry, row: &PgRow) -> Result<LoadedEvent, LoadError> {
    let path: String = row.try_get("event_type").map_err(backend)?;
    let Some(event_type) = event_types.resolve(&path) else {
        return Err(backend(UnknownEventType { path }));
    };

    let sequence: i64 = row.try_get("sequence_number").map_err(backend)?;
    let global_sequence: i64 = row.try_get("global_sequence").map_err(backend)?;
    let payload: Vec<u8> = row.try_get("payload").map_err(backend)?;
    let occurred_at: OffsetDateTime = row.try_get("occurred_at").map_err(backend)?;

    Ok(LoadedEvent {
        stream_key: row.try_get("persistence_id").map_err(backend)?,
        sequence: numbering::from_bigint("sequence_number", sequence).map_err(backend)?,
        global_sequence: numbering::from_bigint("global_sequence", global_sequence)
            .map_err(backend)?,
        event_type,
        payload: Bytes::from(payload),
        occurred_at: timestamp::from_column(occurred_at).map_err(backend)?,
    })
}

fn backend<E>(cause: E) -> LoadError
where
    E: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    LoadError::Backend(cause.into())
}
