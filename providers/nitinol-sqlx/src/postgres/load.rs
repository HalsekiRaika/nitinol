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
use sqlx::{Execute as _, PgPool, Postgres, QueryBuilder};

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
    let mut statement = Conditions::of(query)?.into_statement();

    // A query built from the builder borrows it, and the stream handed back
    // outlives this function, so the text and the values bound into it are
    // taken out and given to a query that owns them.
    let arguments = statement
        .build()
        .take_arguments()
        .map_err(|cause| Failure::from(sqlx::Error::Encode(cause)))?
        .expect("a builder built once hands over the values bound into it");
    let rows = sqlx::query_as_with::<_, StoredEvent, _>(statement.sql(), arguments).fetch(pool);

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

    /// The statement asking for these conditions and for nothing the query
    /// left out.
    ///
    /// Each shape of query is a statement of its own, prepared and kept by the
    /// connection under its own text, so whatever plan the database settles
    /// on for it is a plan for the conditions it holds. A condition written
    /// as one that may be absent cannot be costed: once the database stops
    /// planning such a statement per call, the plan it keeps walks the global
    /// order and filters, which for a stream recovering from a snapshot is
    /// every row of the journal read to return none.
    fn into_statement(self) -> QueryBuilder<Postgres> {
        let mut statement = QueryBuilder::new(
            "SELECT persistence_id, sequence_number, global_sequence, event_type, payload, \
             occurred_at FROM nitinol_journal",
        );
        let mut joint = " WHERE ";

        if let Some(key) = self.stream_key {
            statement
                .push(joint)
                .push("persistence_id = ")
                .push_bind(key);
            joint = " AND ";
        }
        if let Some(from) = self.from_global_sequence {
            statement
                .push(joint)
                .push("global_sequence >= ")
                .push_bind(from);
            joint = " AND ";
        }
        if let Some(from) = self.from_stream_sequence {
            statement
                .push(joint)
                .push("sequence_number >= ")
                .push_bind(from);
            joint = " AND ";
        }
        // An event type prefix matches on segment boundaries: a stored path is
        // under the prefix when it is the prefix itself, or when the prefix is
        // followed by a separator. Comparing the column rather than matching a
        // pattern against it is what keeps `_` and `%` — both legal in a family
        // — from behaving as wildcards.
        //
        // The descendants are asked for as the half-open range `[prefix || '.',
        // prefix || '/')` rather than by taking the leading characters of the
        // column. The two select the same rows: `.` and `/` are adjacent bytes,
        // so a path lies in that range exactly when the prefix is followed by a
        // separator. What differs is that a range over the column reaches an
        // index while a function of the column cannot, and an index is the
        // difference between reading the rows of one declaration and reading
        // every row above the position to throw most of them away.
        //
        // `COLLATE "C"` is what makes that equality hold, and it is not
        // optional: the adjacency of `.` and `/` is a fact about bytes, and
        // under a locale collation the same range leaves out paths that are
        // under the prefix. It names the same collation the index on this
        // column is built with, so the two have to be changed together or the
        // index stops being reachable.
        //
        // A stored path is a family and a type name with nothing after it, so
        // the range reaches a declaration only from a family above it. The
        // equality is what a query for one declaration lands on: it has no
        // descendant of its own for the range to gather.
        if let Some(prefix) = self.event_type_prefix {
            statement
                .push(joint)
                .push(r#"(event_type COLLATE "C" = "#)
                .push_bind(prefix.clone())
                .push(r#" OR (event_type COLLATE "C" >= "#)
                .push_bind(prefix.clone())
                .push(r#" || '.' AND event_type COLLATE "C" < "#)
                .push_bind(prefix)
                .push(" || '/'))");
        }

        statement.push(" ORDER BY global_sequence");

        // A limit whose value the plan does not know is costed as one that
        // stops early, and walking the global order looks cheap behind it, so
        // a query without a limit is not given one to be planned around.
        if let Some(limit) = self.limit {
            statement.push(" LIMIT ").push_bind(limit);
        }

        statement
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
