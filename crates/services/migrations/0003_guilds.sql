-- Guilds (std.guild): durable rows the social role writes through the
-- persistence writer before telling anyone, and reads back after a
-- restart. Membership goes with its guild.

CREATE TABLE guilds (
    id    INTEGER PRIMARY KEY,
    name  TEXT    NOT NULL
);

CREATE UNIQUE INDEX guilds_name ON guilds (lower(name));

CREATE TABLE guild_members (
    character BIGINT   PRIMARY KEY,
    guild     INTEGER  NOT NULL REFERENCES guilds (id) ON DELETE CASCADE,
    rank      SMALLINT NOT NULL,
    since     BIGINT   NOT NULL
);

CREATE INDEX guild_members_guild ON guild_members (guild);

-- The last applied batch of guild changes: a resent batch is applied once.
CREATE TABLE guild_watermark (
    id   INTEGER PRIMARY KEY CHECK (id = 1),
    seq  BIGINT  NOT NULL
);
