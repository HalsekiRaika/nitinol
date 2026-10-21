-- One scope is one row: the sequence that consumer has been carried as far as.
--
-- The scope column holds the key an `OffsetScope` rendered, which is the whole
-- of what a store is told, so one table serves every kind of consumer.
--
-- The primary key is what makes a consumer's offset a single row two writers
-- contend for rather than two rows neither of them sees. A save is an upsert
-- whose update carries a condition, so the comparison that decides whether the
-- offset moves is made by the row lock this key takes and not by a read the
-- writer performed first.
CREATE TABLE nitinol_offset (
    scope    TEXT   NOT NULL,
    sequence BIGINT NOT NULL,
    CONSTRAINT nitinol_offset_pkey PRIMARY KEY (scope)
);
