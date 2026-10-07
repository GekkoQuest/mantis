//! The host library every service role is built on (plan 10): roles,
//! configuration, logging, metrics, health, and typed internal RPC.

pub mod clock;
pub mod rpc;

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use mantis_core::module::toml::{self, Value};

/// A service role. Each runs in its own process in production; local
/// development runs them all in one.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
#[repr(u8)]
pub enum Role {
    /// Authentication, tokens, bans, maintenance mode.
    Account = 1,
    /// Character selection, cell directory, instances, transfer tokens.
    Realm = 2,
    /// Parties, friends, guilds, and cross-cell chat (mail travels as cell
    /// commands and outcomes, not through social).
    Social = 3,
    /// The system of record: log outcomes, the ledger, guild rows, the Ops
    /// audit trail.
    Persist = 4,
    /// Queues and placement.
    Matchmaking = 5,
    /// The audited operations dashboard.
    Ops = 6,
    /// A cell host (game server process).
    Cell = 7,
    /// The game protocol front door (logins).
    Gateway = 8,
}

impl Role {
    /// Every role.
    pub const ALL: [Self; 8] = [
        Self::Account,
        Self::Realm,
        Self::Social,
        Self::Persist,
        Self::Matchmaking,
        Self::Ops,
        Self::Cell,
        Self::Gateway,
    ];

    /// From its wire byte.
    #[must_use]
    pub fn from_u8(b: u8) -> Option<Self> {
        Self::ALL.into_iter().find(|r| *r as u8 == b)
    }

    /// Lower-case name.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Account => "account",
            Self::Realm => "realm",
            Self::Social => "social",
            Self::Persist => "persist",
            Self::Matchmaking => "matchmaking",
            Self::Ops => "ops",
            Self::Cell => "cell",
            Self::Gateway => "gateway",
        }
    }
}

/// Service configuration: a TOML-subset file of `[role]` tables, read
/// strictly (see `mantis_core::module::toml`).
#[derive(Clone, Debug, Default)]
pub struct Config {
    values: BTreeMap<(String, String), Value>,
}

impl Config {
    /// Parses configuration text.
    ///
    /// # Errors
    /// The syntax error.
    pub fn parse(text: &str) -> Result<Self, String> {
        let doc = toml::parse(text).map_err(|e| e.to_string())?;
        let mut values = BTreeMap::new();
        for t in doc.tables {
            for e in t.entries {
                values.insert((t.name.clone(), e.key), e.value);
            }
        }
        Ok(Self { values })
    }

    /// A string value.
    #[must_use]
    pub fn str(&self, table: &str, key: &str) -> Option<&str> {
        self.values
            .get(&(table.to_owned(), key.to_owned()))
            .and_then(Value::as_str)
    }

    /// An integer value.
    #[must_use]
    pub fn int(&self, table: &str, key: &str) -> Option<i64> {
        match self.values.get(&(table.to_owned(), key.to_owned())) {
            Some(Value::Int(i)) => Some(*i),
            _ => None,
        }
    }

    /// A boolean value.
    #[must_use]
    pub fn bool(&self, table: &str, key: &str) -> Option<bool> {
        self.values
            .get(&(table.to_owned(), key.to_owned()))
            .and_then(Value::as_bool)
    }
}

/// A structured log line.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct LogLine {
    /// Unix time in milliseconds.
    pub at_ms: u64,
    /// The role logging.
    pub role: Role,
    /// Level: `info`, `warn`, `error`, `audit`.
    pub level: &'static str,
    /// Message.
    pub message: String,
}

/// A shared, bounded log every role writes to (printed or shipped by the
/// process).
#[derive(Clone, Default)]
pub struct Logger {
    lines: Arc<Mutex<Vec<LogLine>>>,
}

/// Wall-clock Unix milliseconds, for log lines. Roles and links read their
/// injected [`clock::ServiceClock`] instead (cells never call either; their
/// time is the tick).
pub(crate) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// Locks `m`, recovering the data from a poisoned lock: every role's state
/// stays usable after a panicking handler (which the RPC layer reports).
pub(crate) fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// A handler's refusal, with its reason.
pub(crate) fn refused(why: &str) -> rpc::RpcError {
    rpc::RpcError::Refused(why.to_owned())
}

impl Logger {
    /// Logs a line.
    pub fn log(&self, role: Role, level: &'static str, message: impl Into<String>) {
        let mut lines = lock(&self.lines);
        if lines.len() >= 65_536 {
            lines.remove(0);
        }
        lines.push(LogLine {
            at_ms: now_ms(),
            role,
            level,
            message: message.into(),
        });
    }

    /// Takes every line logged so far.
    #[must_use]
    pub fn drain(&self) -> Vec<LogLine> {
        std::mem::take(&mut *lock(&self.lines))
    }
}

/// Counters and gauges by name.
#[derive(Clone, Default)]
pub struct Metrics {
    values: Arc<Mutex<BTreeMap<String, i64>>>,
}

impl Metrics {
    /// Adds to a counter.
    pub fn add(&self, name: &str, n: i64) {
        *lock(&self.values).entry(name.to_owned()).or_insert(0) += n;
    }

    /// Sets a gauge.
    pub fn set(&self, name: &str, v: i64) {
        lock(&self.values).insert(name.to_owned(), v);
    }

    /// One value.
    #[must_use]
    pub fn get(&self, name: &str) -> i64 {
        lock(&self.values).get(name).copied().unwrap_or(0)
    }

    /// Text exposition: `name value` per line, sorted.
    #[must_use]
    pub fn render(&self) -> String {
        let mut out = String::new();
        for (k, v) in lock(&self.values).iter() {
            let _ = writeln!(out, "{k} {v}");
        }
        out
    }
}

/// A role's health.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Health {
    /// Starting: not ready for traffic.
    Starting,
    /// Serving.
    Ready,
    /// Serving with a problem.
    Degraded,
    /// Not serving.
    Down,
}

/// Health of every role in this process.
#[derive(Clone, Default)]
pub struct HealthBoard {
    roles: Arc<Mutex<BTreeMap<Role, (Health, String)>>>,
}

impl HealthBoard {
    /// Reports a role's health.
    pub fn report(&self, role: Role, health: Health, note: impl Into<String>) {
        lock(&self.roles).insert(role, (health, note.into()));
    }

    /// A role's health.
    #[must_use]
    pub fn of(&self, role: Role) -> Option<Health> {
        lock(&self.roles).get(&role).map(|(h, _)| *h)
    }
}

/// The default RPC timeout between roles.
pub const RPC_TIMEOUT: Duration = Duration::from_secs(2);

/// The longest wait between attempts to make a role's change durable.
pub const DURABLE_RETRY: Duration = Duration::from_secs(1);

/// One term of an instance's hold on its role's lease (role failover),
/// shared by the lease task and the service it seats: the epoch the
/// service's durable writes carry, and whether the term is over. An
/// unleased, single-instance role's fence is epoch 0 and never lost.
#[derive(Clone, Debug, Default)]
pub struct Fence(Arc<FenceState>);

#[derive(Debug, Default)]
struct FenceState {
    epoch: AtomicU64,
    lost: AtomicBool,
}

impl Fence {
    /// An unleased role's fence: epoch 0.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A term under lease `epoch`.
    #[must_use]
    pub fn held(epoch: u64) -> Self {
        let f = Self::default();
        f.0.epoch.store(epoch, Ordering::Release);
        f
    }

    /// The epoch writes carry.
    #[must_use]
    pub fn epoch(&self) -> u64 {
        self.0.epoch.load(Ordering::Acquire)
    }

    /// Ends the term: the lease was lost, or a write was refused as stale.
    /// The instance is a standby from now on.
    pub fn lose(&self) {
        self.0.lost.store(true, Ordering::Release);
    }

    /// True once the term is over.
    #[must_use]
    pub fn is_lost(&self) -> bool {
        self.0.lost.load(Ordering::Acquire)
    }
}

/// Calls `M` with `req` until the persistence writer answers (the same
/// numbered batch every time: the writer applies it once).
///
/// # Errors
/// [`rpc::RpcError::Standby`] once `fence`'s term is over (a stale-epoch
/// refusal ends it): nothing more is written, and the caller answers as a
/// standby.
pub(crate) async fn until_durable<M: rpc::Method>(
    writer: &rpc::RpcClient,
    fence: &Fence,
    req: &M::Request,
) -> Result<(), rpc::RpcError> {
    let mut delay = Duration::from_millis(20);
    loop {
        if fence.is_lost() {
            return Err(rpc::RpcError::Standby);
        }
        match writer.call::<M>(req, RPC_TIMEOUT).await {
            Ok(_) => return Ok(()),
            Err(rpc::RpcError::StaleEpoch) => {
                fence.lose();
                return Err(rpc::RpcError::Standby);
            }
            Err(_) => {
                tokio::time::sleep(delay).await;
                delay = (delay * 2).min(DURABLE_RETRY);
            }
        }
    }
}

/// The operating system's secure random source.
pub struct Random(pub ring::rand::SystemRandom);

impl Default for Random {
    fn default() -> Self {
        Self(ring::rand::SystemRandom::new())
    }
}
