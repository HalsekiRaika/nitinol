CREATE TABLE journal(
    id           TEXT     NOT NULL,
    sequence_id  INTEGER  NOT NULL,
    registry_key TEXT     NOT NULL,
    bytes        BLOB     NOT NULL,
    created_at   DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,

    PRIMARY KEY (id, sequence_id)
);
