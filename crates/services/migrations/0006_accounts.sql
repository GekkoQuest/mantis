-- Accounts (the account role): durable rows the role writes through the
-- persistence writer before it answers, and reads back at start, so
-- registrations, password changes, last logins and bans survive a restart.
-- Session tokens are not stored: they are ephemeral by design.

CREATE TABLE accounts (
    id               BIGINT  PRIMARY KEY,
    name             TEXT    NOT NULL,
    -- The name lower-cased: names are unique case-insensitively.
    name_key         TEXT    NOT NULL UNIQUE,
    salt             BYTEA   NOT NULL,
    hash             BYTEA   NOT NULL,
    iterations       INTEGER NOT NULL,
    created_ms       BIGINT  NOT NULL,
    last_login_ms    BIGINT  NOT NULL,
    banned_until_ms  BIGINT  NOT NULL,
    ban_reason       TEXT    NOT NULL
);

-- The last applied batch of account rows: a resent batch is applied once.
CREATE TABLE account_watermark (
    id   INTEGER PRIMARY KEY CHECK (id = 1),
    seq  BIGINT  NOT NULL
);
