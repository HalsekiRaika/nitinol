-- One event is one row, and a stream's sequence numbers are unique within it.
-- The primary key is the race-free half of the OCC contract: a second append of
-- a `(persistence_id, sequence_number)` pair the journal already holds is
-- refused by the index rather than by a check the writer performs first.
--
-- `occurred_at` is the instant the caller handed over; `created_at` is when the
-- row reached this database. They answer different questions, so both are kept
-- and neither stands in for the other.
CREATE TABLE nitinol_journal (
    persistence_id  TEXT        NOT NULL,
    sequence_number BIGINT      NOT NULL,
    global_sequence BIGINT      NOT NULL,
    event_type      TEXT        NOT NULL,
    payload         BYTEA       NOT NULL,
    occurred_at     TIMESTAMPTZ NOT NULL,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    CONSTRAINT nitinol_journal_pkey PRIMARY KEY (persistence_id, sequence_number)
);

-- A reader following the store's global order asks for a range of
-- `global_sequence` and reads it in that order. The index serves both, and its
-- uniqueness makes a numbering that handed one value out twice a failure at the
-- write rather than a duplicate a reader has to notice.
CREATE UNIQUE INDEX nitinol_journal_global_sequence_key
    ON nitinol_journal (global_sequence);

-- The global numbering is a row rather than a sequence object, because a
-- sequence does not roll back. A refused append must leave the numbering where
-- it found it, and only a counter that is part of the transaction does that.
CREATE TABLE nitinol_global_sequence (
    singleton  BOOLEAN PRIMARY KEY CHECK (singleton),
    next_value BIGINT NOT NULL
);

INSERT INTO nitinol_global_sequence (singleton, next_value) VALUES (TRUE, 1);
