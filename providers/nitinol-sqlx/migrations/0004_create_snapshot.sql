-- One aggregate is one row: the state it had at the position that row names.
--
-- A snapshot is a shortcut and never a record — the journal is — so there is no
-- history to keep here. The primary key over the aggregate alone is what makes
-- a save replace the row before it rather than accumulate beside it, and a
-- table that kept every snapshot ever taken would grow without bound for
-- something a replay can always rebuild.
--
-- The save is an upsert whose update carries no condition, so whichever save
-- came last is what a load answers with — including a save at a position the
-- stored row has already reached, or behind it. That is the answer the
-- in-memory store gives, so a deployment moving between the two finds its
-- snapshots behaving the same way for it.
--
-- `created_at` is the instant the caller took the snapshot at, the way the
-- journal's `occurred_at` is the instant an event happened at. It has no
-- default: a row written by a save carries the value that save handed over, not
-- the moment the row reached this database.
--
-- Every column is required. A nullable one would allow a row no save ever wrote
-- whole, and a state beside a position it does not belong to is the one reading
-- of a snapshot that loses events with no error raised anywhere.
CREATE TABLE nitinol_snapshot (
    aggregate_id TEXT        NOT NULL,
    sequence     BIGINT      NOT NULL,
    payload      BYTEA       NOT NULL,
    created_at   TIMESTAMPTZ NOT NULL,
    CONSTRAINT nitinol_snapshot_pkey PRIMARY KEY (aggregate_id)
);
