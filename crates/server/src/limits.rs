//! Per-session rate limits (plan 12): token buckets per message kind and a
//! byte budget, enforced by the host before anything reaches a cell's
//! inbox, so a flood never costs the cell a tick.
//!
//! Buckets refill once per host poll (a tick boundary) in thousandths of a
//! message, so a rate that does not divide the tick rate is exact over a
//! second. A refused message is dropped and counted against the session;
//! the count reaches the cell's cheat counter as a logged intent and, past
//! the package's threshold, the session is ended.

use std::collections::BTreeMap;

use mantis_adapter_contract::Inbound;

/// One token bucket: a sustained rate and a burst.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Bucket {
    /// Messages per second, sustained.
    pub per_second: u32,
    /// Messages allowed at once (the bucket's size).
    pub burst: u32,
}

/// A package's limits (its `[tunables.limits]` table).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct RateLimits {
    /// Movement inputs and claims.
    pub inputs: Bucket,
    /// Casts, interactions, and choices.
    pub actions: Bucket,
    /// Extension messages, per extension kind.
    pub extensions: Bucket,
    /// Snapshot acknowledgements.
    pub acks: Bucket,
    /// Bytes received per second.
    pub bytes_per_second: u32,
    /// Refused messages after which the session is ended.
    pub kick_after: u32,
}

impl RateLimits {
    /// Generous limits for an engine without a package: four times a
    /// 30 Hz client's inputs.
    pub const DEFAULT: Self = Self {
        inputs: Bucket {
            per_second: 120,
            burst: 60,
        },
        actions: Bucket {
            per_second: 20,
            burst: 20,
        },
        extensions: Bucket {
            per_second: 20,
            burst: 20,
        },
        acks: Bucket {
            per_second: 120,
            burst: 60,
        },
        bytes_per_second: 65_536,
        kick_after: 1_000,
    };
}

/// Which bucket a message draws from.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Kind {
    /// Inputs and claims.
    Input,
    /// Casts, interactions, choices.
    Action,
    /// An extension message of this kind.
    Extension(u16),
    /// Snapshot acknowledgements.
    Ack,
}

impl Kind {
    /// The bucket of `m`, or `None` for messages that are not limited
    /// (the handshake, goodbye).
    #[must_use]
    pub fn of(m: &Inbound) -> Option<Self> {
        Some(match m {
            Inbound::Move(_) | Inbound::MoveClaim(_) => Self::Input,
            Inbound::Cast(_) | Inbound::Interact(_) | Inbound::Choose(_) => Self::Action,
            Inbound::Extension(x) => Self::Extension(x.kind.0),
            Inbound::SnapshotAck(_) => Self::Ack,
            Inbound::Hello(_) | Inbound::Goodbye(_) => return None,
        })
    }

    fn bucket(self, limits: &RateLimits) -> Bucket {
        match self {
            Self::Input => limits.inputs,
            Self::Action => limits.actions,
            Self::Extension(_) => limits.extensions,
            Self::Ack => limits.acks,
        }
    }
}

/// One session's buckets and counters.
#[derive(Clone, Debug, Default)]
pub struct SessionLimiter {
    /// Kind -> (thousandths of a message available, poll last refilled).
    buckets: BTreeMap<Kind, (u64, u64)>,
    window: u64,
    bytes: u64,
    /// Messages and frames refused so far.
    pub violations: u32,
    /// Refusals not yet reported to the session's cell.
    pub unreported: u32,
}

impl SessionLimiter {
    /// True when a message of `kind` may pass at host poll `poll`.
    pub fn admit(&mut self, kind: Kind, poll: u64, tick_rate: u32, limits: &RateLimits) -> bool {
        let b = kind.bucket(limits);
        let cap = u64::from(b.burst).saturating_mul(1000);
        let per_poll = u64::from(b.per_second).saturating_mul(1000) / u64::from(tick_rate.max(1));
        let (tokens, last) = self.buckets.entry(kind).or_insert((cap, poll));
        let elapsed = poll.saturating_sub(*last);
        *tokens = tokens.saturating_add(per_poll.saturating_mul(elapsed)).min(cap);
        *last = poll;
        if *tokens >= 1000 {
            *tokens -= 1000;
            true
        } else {
            self.refuse();
            false
        }
    }

    /// True when `n` more bytes fit this second's budget.
    pub fn bytes(&mut self, n: usize, poll: u64, tick_rate: u32, limits: &RateLimits) -> bool {
        let second = poll / u64::from(tick_rate.max(1));
        if second != self.window {
            self.window = second;
            self.bytes = 0;
        }
        self.bytes = self.bytes.saturating_add(n as u64);
        if self.bytes <= u64::from(limits.bytes_per_second) {
            true
        } else {
            self.refuse();
            false
        }
    }

    fn refuse(&mut self) {
        self.violations = self.violations.saturating_add(1);
        self.unreported = self.unreported.saturating_add(1);
    }
}
