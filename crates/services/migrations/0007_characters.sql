-- Characters (the realm): durable rows the realm writes through the
-- persistence writer before it answers, and reads back at start. A
-- character's state in the world stays in its cell's snapshots and
-- outcomes; this is the realm's summary for selection and placement: the
-- cell, world and position it was last in, and its level. Entry and
-- transfer tokens are not stored: they are ephemeral by design.

CREATE TABLE characters (
    id          BIGINT   PRIMARY KEY,
    account     BIGINT   NOT NULL,
    name        TEXT     NOT NULL,
    -- The name lower-cased, among living characters unique.
    name_key    TEXT     NOT NULL,
    kind        INTEGER  NOT NULL,
    created_ms  BIGINT   NOT NULL,
    -- A deleted character keeps its row so its id is never reused.
    deleted     BOOLEAN  NOT NULL,
    cell        BIGINT   NOT NULL,
    world       INTEGER  NOT NULL,
    x           REAL     NOT NULL,
    y           REAL     NOT NULL,
    z           REAL     NOT NULL,
    level       INTEGER  NOT NULL
);

CREATE UNIQUE INDEX characters_name_key ON characters (name_key) WHERE NOT deleted;
CREATE INDEX characters_account ON characters (account);

-- The last applied batch of character rows: a resent batch is applied once.
CREATE TABLE character_watermark (
    id   INTEGER PRIMARY KEY CHECK (id = 1),
    seq  BIGINT  NOT NULL
);
