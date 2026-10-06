-- Friend lists and open friend requests (std.friends): durable rows the
-- social role writes through the persistence writer before telling anyone,
-- and reads back after a restart, so friendships between characters that
-- are both offline survive it.

-- Each friendship once, the lower character id first.
CREATE TABLE friendships (
    a  BIGINT NOT NULL,
    b  BIGINT NOT NULL,
    PRIMARY KEY (a, b),
    CHECK (a < b)
);

CREATE INDEX friendships_b ON friendships (b);

CREATE TABLE friend_requests (
    asker  BIGINT NOT NULL,
    asked  BIGINT NOT NULL,
    PRIMARY KEY (asker, asked)
);

-- The last applied batch of friend changes: a resent batch is applied once.
CREATE TABLE friend_watermark (
    id   INTEGER PRIMARY KEY CHECK (id = 1),
    seq  BIGINT  NOT NULL
);
