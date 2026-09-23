-- One projection is one row: the sequence it has been carried as far as.
--
-- The primary key is what makes a projection's checkpoint a single row two
-- writers contend for rather than two rows neither of them sees. A save is an
-- upsert whose update carries a condition, so the comparison that decides
-- whether the checkpoint moves is made by the row lock this key takes and not
-- by a read the writer performed first.
CREATE TABLE nitinol_checkpoint (
    projection_id TEXT   NOT NULL,
    sequence      BIGINT NOT NULL,
    CONSTRAINT nitinol_checkpoint_pkey PRIMARY KEY (projection_id)
);
