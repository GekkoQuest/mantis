-- The system of record (decision 0005): outcomes as the cells logged them,
-- the per-cell batch watermark that makes pushes idempotent, the ledger
-- (partitioned by month), and the Ops audit trail.

CREATE TABLE outcomes (
    cell     BIGINT  NOT NULL,
    batch    BIGINT  NOT NULL,
    idx      INTEGER NOT NULL,
    tick     BIGINT  NOT NULL,
    at_ms    BIGINT  NOT NULL,
    kind     INTEGER NOT NULL,
    session  BIGINT  NOT NULL,
    ok       BOOLEAN NOT NULL,
    payload  BYTEA   NOT NULL,
    PRIMARY KEY (cell, batch, idx)
);

CREATE TABLE batches (
    cell       BIGINT PRIMARY KEY,
    last_batch BIGINT NOT NULL
);

CREATE TABLE ledger (
    month     INTEGER NOT NULL,
    cell      BIGINT  NOT NULL,
    tick      BIGINT  NOT NULL,
    character BIGINT  NOT NULL,
    item      BIGINT  NOT NULL,
    delta     BIGINT  NOT NULL,
    at_ms     BIGINT  NOT NULL
) PARTITION BY LIST (month);

CREATE INDEX ledger_character ON ledger (character);

CREATE TABLE ops_audit (
    id       BIGSERIAL PRIMARY KEY,
    at_ms    BIGINT NOT NULL,
    actor    TEXT   NOT NULL,
    command  TEXT   NOT NULL,
    args     TEXT   NOT NULL,
    status   TEXT   NOT NULL,
    before   TEXT,
    after    TEXT,
    undo     TEXT
);
