-- Live values (flags and tunables Ops set while cells run): durable, so a
-- restarted Ops publishes every current value again (plan 10: live
-- tunables are versioned and pushed again).

CREATE TABLE live_values (
    name  TEXT     PRIMARY KEY,
    kind  SMALLINT NOT NULL,
    value REAL     NOT NULL,
    -- The order the values were last set in.
    seq   BIGINT   NOT NULL
);
