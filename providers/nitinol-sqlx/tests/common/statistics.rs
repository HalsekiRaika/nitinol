//! What the database counted against the journal, for a clause about how a
//! statement the provider issued was served.
//!
//! The counters are the database's own, so what they measure is the statement
//! the provider itself ran rather than a copy of it. They are read on the same
//! single connection the statement ran on — see `MigratedSchema::pinned_pool`
//! for why.

use sqlx::PgPool;

/// What the database counted against one index.
pub struct IndexReads {
    /// How many times a scan of this index was started.
    pub scans: i64,
    /// How many index entries those scans returned.
    pub entries: i64,
}

/// Publish what this connection has done so far to the shared counters.
///
/// A backend holds its own tally and reports it when a transaction of its own
/// ends, and not more often than once a second unless asked. Asking is what
/// makes a reading taken straight after a statement a reading of that
/// statement.
pub async fn publish_statistics(pool: &PgPool) {
    sqlx::query("SELECT pg_stat_force_next_flush()")
        .execute(pool)
        .await
        .expect("a database this run reached publishes its own activity counters");
}

/// What has been read off `index` so far.
pub async fn index_reads(pool: &PgPool, index: &str) -> IndexReads {
    let row: Option<(i64, i64)> = sqlx::query_as(
        r#"
        SELECT coalesce(idx_scan, 0), coalesce(idx_tup_read, 0)
          FROM pg_stat_all_indexes
         WHERE schemaname = current_schema() AND indexrelname = $1
        "#,
    )
    .bind(index)
    .fetch_optional(pool)
    .await
    .expect("the statistics views answer on a database this run reached");

    let (scans, entries) = row.unwrap_or_else(|| {
        panic!("the provider's migrations must create {index}, and the schema holds no such index")
    });
    IndexReads { scans, entries }
}

/// How many rows the journal has been read sequentially.
pub async fn sequential_rows_read(pool: &PgPool) -> i64 {
    sqlx::query_scalar(
        r#"
        SELECT coalesce(seq_tup_read, 0)
          FROM pg_stat_all_tables
         WHERE schemaname = current_schema() AND relname = 'nitinol_journal'
        "#,
    )
    .fetch_one(pool)
    .await
    .expect("the statistics views answer for a table this run migrated")
}

/// Let the planner see what was seeded.
///
/// Without it the planner is choosing against the estimates of an empty table,
/// so what a plan did would say nothing about an index.
pub async fn analyze(pool: &PgPool) {
    sqlx::query("ANALYZE nitinol_journal")
        .execute(pool)
        .await
        .expect("a migrated schema can be analyzed");
}
