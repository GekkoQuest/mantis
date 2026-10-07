-- Role leases (role failover): one row per service role naming its one
-- active instance. The active renews its lease; a standby takes it over once
-- it lapses, with the next epoch. Durable writes carry the writer's epoch and
-- are refused unless it is the role's current one, so an instance that lost
-- its lease cannot write twice.

CREATE TABLE role_leases (
    role        SMALLINT PRIMARY KEY,
    owner       TEXT     NOT NULL,
    epoch       BIGINT   NOT NULL,
    expires_ms  BIGINT   NOT NULL
);
