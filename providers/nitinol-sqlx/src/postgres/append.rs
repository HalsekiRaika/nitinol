//! Taking an append into the journal, and the numbering it is given.
//!
//! Two obligations meet in one transaction here.
//!
//! Within a stream, the primary key over `(persistence_id, sequence_number)` is
//! what refuses a sequence the stream already holds. Nothing is read first and
//! then checked: the index decides, so two writers racing for one sequence
//! cannot both be told they won.
//!
//! Across the store, OCC-5 requires that numbering and becoming visible happen
//! in the one order — an event numbered before a commit that lands after a
//! reader has passed that number would be stored, intact, and read by nobody.
//! An append therefore takes an advisory lock that is held until its
//! transaction ends, so the numbering it takes and the commit that publishes it
//! are one serialised step. The counter it takes from is a row rather than a
//! sequence object, so a transaction that ends in a refusal gives its numbering
//! back: a refused append consumes none of it.

use nitinol_persistence::error::AppendError;
use nitinol_persistence::{AppendOutcome, AppendingEvent};
use sqlx::PgPool;

use super::failure::{self, Failure};
use super::numbering;
use super::timestamp::Instant;

/// The lock every append against this journal takes before it numbers
/// anything.
///
/// The value itself carries no meaning and is never read; all that matters is
/// that every append picks the same one, so that no two of them number and
/// commit at the same time.
const NUMBERING_LOCK: i64 = 0x6E69_7469_6E6F_6C01;

pub(super) async fn append(
    pool: &PgPool,
    key: &str,
    events: Vec<AppendingEvent>,
) -> Result<AppendOutcome, AppendError> {
    store(pool, key, events)
        .await
        .map_err(|failure| failure::appending(key, failure))
}

async fn store(
    pool: &PgPool,
    key: &str,
    events: Vec<AppendingEvent>,
) -> Result<AppendOutcome, Failure> {
    // An empty batch stores nothing, so it opens no transaction and takes no
    // numbering; what it reports is the sequence the stream had already
    // reached.
    let Some(stream_version) = events.iter().map(|event| event.sequence).max() else {
        return sequence_reached(pool, key).await;
    };

    let columns = Columns::of(&events)?;

    let mut appending = pool.begin().await?;

    sqlx::query(r#"SELECT pg_advisory_xact_lock($1)"#)
        .bind(NUMBERING_LOCK)
        .execute(&mut *appending)
        .await?;

    // Take as many numbers as the batch has events, and report the first of
    // them. The count comes from the same array the rows are built out of, so
    // what is taken and what is stored cannot drift apart.
    let first_global_sequence: i64 = sqlx::query_scalar(
        r#"
        UPDATE nitinol_global_sequence
           SET next_value = next_value + cardinality($1::bigint[])
         WHERE singleton
        RETURNING next_value - cardinality($1::bigint[]) AS first_global_sequence
        "#,
    )
    .bind(columns.sequence_numbers.as_slice())
    .fetch_one(&mut *appending)
    .await?;

    // Store the batch in the order it was given, numbering it from the first
    // value the counter handed over. There is no `ON CONFLICT` clause: a
    // conflict is the answer this append owes its caller, and swallowing it
    // would leave a batch carrying one sequence twice quietly half-stored.
    let stored: Vec<i64> = sqlx::query_scalar(
        r#"
        INSERT INTO nitinol_journal
            (persistence_id, sequence_number, global_sequence, event_type, payload, occurred_at)
        SELECT $1,
               appended.sequence_number,
               $2 + appended.ordinal - 1,
               appended.event_type,
               appended.payload,
               appended.occurred_at
          FROM unnest($3::bigint[], $4::text[], $5::bytea[], $6::timestamptz[])
               WITH ORDINALITY
               AS appended(sequence_number, event_type, payload, occurred_at, ordinal)
        RETURNING global_sequence
        "#,
    )
    .bind(key)
    .bind(first_global_sequence)
    .bind(columns.sequence_numbers.as_slice())
    .bind(columns.event_types.as_slice())
    .bind(columns.payloads.as_slice())
    .bind(columns.occurred_at.as_slice())
    .fetch_all(&mut *appending)
    .await?;

    appending.commit().await?;

    Ok(AppendOutcome {
        assigned_sequences: assigned(stored)?,
        stream_version,
    })
}

/// The sequence `key` has reached, for an append that stores nothing.
async fn sequence_reached(pool: &PgPool, key: &str) -> Result<AppendOutcome, Failure> {
    let reached: i64 = sqlx::query_scalar(
        r#"
        SELECT COALESCE(MAX(sequence_number), 0)
          FROM nitinol_journal
         WHERE persistence_id = $1
        "#,
    )
    .bind(key)
    .fetch_one(pool)
    .await?;

    Ok(AppendOutcome {
        assigned_sequences: Vec::new(),
        stream_version: numbering::from_bigint("sequence_number", reached)?,
    })
}

/// What the batch was numbered, in the order it was appended in.
///
/// Each event is numbered from its position in the batch, so ascending
/// `global_sequence` is that order; sorting is what makes the reported order
/// independent of the order the rows came back in.
fn assigned(mut stored: Vec<i64>) -> Result<Vec<u64>, Failure> {
    stored.sort_unstable();
    Ok(stored
        .into_iter()
        .map(|global_sequence| numbering::from_bigint("global_sequence", global_sequence))
        .collect::<Result<Vec<u64>, _>>()?)
}

/// The batch as the columns it is stored in, each in the order it was appended
/// in.
struct Columns {
    sequence_numbers: Vec<i64>,
    event_types: Vec<String>,
    payloads: Vec<Vec<u8>>,
    occurred_at: Vec<Instant>,
}

impl Columns {
    fn of(events: &[AppendingEvent]) -> Result<Self, Failure> {
        let mut columns = Self {
            sequence_numbers: Vec::with_capacity(events.len()),
            event_types: Vec::with_capacity(events.len()),
            payloads: Vec::with_capacity(events.len()),
            occurred_at: Vec::with_capacity(events.len()),
        };

        for event in events {
            columns
                .sequence_numbers
                .push(numbering::to_bigint("sequence_number", event.sequence)?);
            columns
                .event_types
                .push(event.event_type.to_path().to_string());
            columns.payloads.push(event.payload.to_vec());
            columns
                .occurred_at
                .push(Instant::try_from(event.occurred_at)?);
        }

        Ok(columns)
    }
}
