//! Account: authentication, session tokens, bans, maintenance mode.
//!
//! Passwords are stretched with PBKDF2-HMAC-SHA256 and a per-account random
//! salt; only the derived key is kept, with its iteration count. A session
//! token is 32 random bytes, valid for five minutes and for one
//! verification (a cell consumes it at the handshake).
//!
//! Accounts are durable through the persistence writer
//! ([`AccountService::with_writer`]): a registration, a password change, a
//! login (its time) and a ban are written, as whole account rows in a
//! numbered batch the writer applies once, before the role answers, and
//! only then become what logins read. A restarted role reads every row back
//! ([`AccountService::load_durable`]) before it serves. Session tokens and
//! maintenance mode are not durable: a token issued before a restart is
//! refused after it, and the client logs in again.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::num::NonZeroU32;
use std::sync::{Arc, Mutex, MutexGuard};

use mantis_core::wire::BoundedArray;
use ring::rand::SecureRandom;
use ring::{digest, pbkdf2};

use crate::generated::services as m;
use crate::host::clock::ServiceClock;
use crate::host::rpc::{Router, RpcClient, RpcError};
use crate::host::{RPC_TIMEOUT, Role, refused, until_durable};
use crate::methods;
use crate::persist::{AccountRecord, account_record, account_row, name_key};

/// How long a session token stays valid, in milliseconds.
pub const TOKEN_MS: u64 = 5 * 60 * 1000;

/// PBKDF2 iterations for new and changed passwords (a row keeps its own).
const ITERATIONS: u32 = 10_000;

#[derive(Default)]
struct State {
    /// The highest account id ever used.
    next: u64,
    /// The last batch of account rows written.
    seq: u64,
    /// Durable accounts by [`name_key`].
    by_key: BTreeMap<String, AccountRecord>,
    tokens: BTreeMap<[u8; 32], (u64, u64)>,
    maintenance: bool,
}

/// The account role.
#[derive(Clone, Default)]
pub struct AccountService {
    state: Arc<Mutex<State>>,
    rng: Arc<crate::host::Random>,
    clock: ServiceClock,
    /// The persistence writer account rows go through (none: in memory
    /// only, for tests of the rules alone).
    writer: Option<Arc<RpcClient>>,
    /// One change at a time from durable write to answer.
    order: Arc<tokio::sync::Mutex<()>>,
}

fn derive(salt: &[u8], password: &str, iterations: u32) -> [u8; 32] {
    let mut key = [0u8; 32];
    let iterations = NonZeroU32::new(iterations).unwrap_or(NonZeroU32::MIN);
    pbkdf2::derive(
        pbkdf2::PBKDF2_HMAC_SHA256,
        iterations,
        salt,
        password.as_bytes(),
        &mut key,
    );
    key
}

fn verify(a: &AccountRecord, password: &str) -> bool {
    pbkdf2::verify(
        pbkdf2::PBKDF2_HMAC_SHA256,
        NonZeroU32::new(a.iterations).unwrap_or(NonZeroU32::MIN),
        &a.salt,
        password.as_bytes(),
        &a.hash,
    )
    .is_ok()
}

impl AccountService {
    /// An empty account store in memory only (tests of the rules alone; a
    /// served role uses [`AccountService::with_writer`]).
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The role writing every account row through `writer` before it
    /// answers. Call [`AccountService::load_durable`] before serving.
    #[must_use]
    pub fn with_writer(writer: RpcClient) -> Self {
        Self {
            writer: Some(Arc::new(writer)),
            ..Self::default()
        }
    }

    /// The same role on `clock` (token lifetimes, bans, creation and login
    /// times).
    #[must_use]
    pub fn clocked(mut self, clock: ServiceClock) -> Self {
        self.clock = clock;
        self
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        crate::host::lock(&self.state)
    }

    /// Reads every durable account back from the writer (after a restart,
    /// before serving); returns how many.
    ///
    /// # Errors
    /// No writer (a served role must have its store), the writer's refusal,
    /// or a malformed row: the role must not serve.
    pub async fn load_durable(&self) -> Result<usize, String> {
        let writer = self
            .writer
            .as_ref()
            .ok_or("the account role has no persistence writer")?;
        let (mut rows, mut seq) = (Vec::new(), 0);
        for page in 0.. {
            // Boxed: a page of rows is large for a stack frame.
            let got =
                Box::pin(writer.call::<methods::LoadAccounts>(&m::ReadAccountRows { page }, RPC_TIMEOUT))
                    .await
                    .map_err(|e| format!("loading accounts: {e}"))?;
            seq = got.seq;
            for r in got.rows.iter() {
                rows.push(account_record(r).ok_or("loading accounts: a malformed row")?);
            }
            if !got.more {
                break;
            }
        }
        let mut s = self.lock();
        s.seq = seq;
        s.next = rows.iter().map(|a| a.id).max().unwrap_or(0);
        s.by_key = rows.into_iter().map(|a| (name_key(&a.name), a)).collect();
        Ok(s.by_key.len())
    }

    /// Makes `row` durable (one numbered batch, retried until the writer
    /// answers), then makes it what the role reads. Call with the order
    /// lock held.
    async fn commit(&self, row: AccountRecord) {
        if let Some(writer) = &self.writer {
            let seq = {
                let mut s = self.lock();
                s.seq += 1;
                s.seq
            };
            let req = m::StoreAccountRows {
                epoch: 0,
                seq,
                rows: BoundedArray::from_slice(&[account_row(&row)]).unwrap_or_default(),
            };
            until_durable::<methods::WriteAccounts>(writer, &req).await;
        }
        let mut s = self.lock();
        s.next = s.next.max(row.id);
        s.by_key.insert(name_key(&row.name), row);
    }

    fn by_name(&self, name: &str) -> Option<AccountRecord> {
        self.lock().by_key.get(&name_key(name)).cloned()
    }

    async fn register(&self, req: &m::Register) -> Result<m::Registered, RpcError> {
        let mut salt = [0u8; 16];
        self.rng.0.fill(&mut salt).map_err(|_| refused("no randomness"))?;
        let hash = derive(&salt, req.password.as_str(), ITERATIONS);
        let _order = self.order.lock().await;
        let id = {
            let s = self.lock();
            if s.by_key.contains_key(&name_key(req.name.as_str())) {
                return Err(refused("name taken"));
            }
            s.next + 1
        };
        self.commit(AccountRecord {
            id,
            name: req.name.as_str().to_owned(),
            salt,
            hash,
            iterations: ITERATIONS,
            created_ms: self.clock.now_ms(),
            last_login_ms: 0,
            banned_until_ms: 0,
            ban_reason: String::new(),
        })
        .await;
        Ok(m::Registered {
            account: m::AccountId(id),
        })
    }

    async fn login(&self, req: &m::Login) -> Result<m::Session, RpcError> {
        if self.lock().maintenance {
            return Err(refused("maintenance"));
        }
        let a = self
            .by_name(req.name.as_str())
            .ok_or_else(|| refused("bad credentials"))?;
        if !verify(&a, req.password.as_str()) {
            return Err(refused("bad credentials"));
        }
        if a.banned_until_ms > self.clock.now_ms() {
            return Err(refused("banned"));
        }
        let mut token = [0u8; 32];
        self.rng
            .0
            .fill(&mut token)
            .map_err(|_| refused("no randomness"))?;
        {
            // The login time is durable before the session is.
            let _order = self.order.lock().await;
            let mut now = self
                .by_name(req.name.as_str())
                .ok_or_else(|| refused("bad credentials"))?;
            now.last_login_ms = self.clock.now_ms();
            self.commit(now).await;
        }
        let expires = self.clock.now_ms() + TOKEN_MS;
        self.lock().tokens.insert(token, (a.id, expires));
        Ok(m::Session {
            account: m::AccountId(a.id),
            token: BoundedArray::from_slice(&token).unwrap_or_default(),
        })
    }

    async fn change_password(&self, req: &m::ChangePassword) -> Result<m::Empty, RpcError> {
        let mut salt = [0u8; 16];
        self.rng.0.fill(&mut salt).map_err(|_| refused("no randomness"))?;
        let hash = derive(&salt, req.new.as_str(), ITERATIONS);
        let _order = self.order.lock().await;
        let mut a = self
            .by_name(req.name.as_str())
            .ok_or_else(|| refused("bad credentials"))?;
        if !verify(&a, req.old.as_str()) {
            return Err(refused("bad credentials"));
        }
        a.salt = salt;
        a.hash = hash;
        a.iterations = ITERATIONS;
        self.commit(a).await;
        Ok(m::Empty {})
    }

    fn verify_token(&self, req: &m::VerifyToken) -> Result<m::Verified, RpcError> {
        let bytes: Vec<u8> = req.token.iter().copied().collect();
        let token: [u8; 32] = bytes.try_into().map_err(|_| refused("bad token"))?;
        let (id, expires) = self
            .lock()
            .tokens
            .remove(&token)
            .ok_or_else(|| refused("bad token"))?;
        if expires < self.clock.now_ms() {
            return Err(refused("expired token"));
        }
        Ok(m::Verified {
            account: m::AccountId(id),
        })
    }

    async fn ban(&self, req: &m::Ban) -> Result<m::Banned, RpcError> {
        let _order = self.order.lock().await;
        let mut a = self
            .lock()
            .by_key
            .values()
            .find(|a| a.id == req.account.0)
            .cloned()
            .ok_or_else(|| refused("no such account"))?;
        let previous_until_ms = std::mem::replace(&mut a.banned_until_ms, req.until_ms);
        a.ban_reason = if req.until_ms == 0 {
            String::new()
        } else {
            req.reason.as_str().to_owned()
        };
        self.commit(a).await;
        Ok(m::Banned { previous_until_ms })
    }

    /// True when maintenance mode is on.
    #[must_use]
    pub fn maintenance(&self) -> bool {
        self.lock().maintenance
    }

    /// When `account` is banned until (0: not banned).
    #[must_use]
    pub fn banned_until(&self, account: u64) -> Option<u64> {
        self.account(account).map(|a| a.banned_until_ms)
    }

    /// The durable row of `account` (Ops and tests).
    #[must_use]
    pub fn account(&self, account: u64) -> Option<AccountRecord> {
        self.lock().by_key.values().find(|a| a.id == account).cloned()
    }

    /// Accounts held (Ops and tests).
    #[must_use]
    pub fn len(&self) -> usize {
        self.lock().by_key.len()
    }

    /// No accounts.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// A short fingerprint of the account store (Ops before/after rows).
    #[must_use]
    pub fn fingerprint(&self) -> String {
        let s = self.lock();
        let mut ctx = digest::Context::new(&digest::SHA256);
        for (key, a) in &s.by_key {
            ctx.update(key.as_bytes());
            ctx.update(&a.banned_until_ms.to_le_bytes());
        }
        ctx.update(&[u8::from(s.maintenance)]);
        let digest = ctx.finish();
        let mut out = String::new();
        for b in digest.as_ref().iter().take(6) {
            let _ = write!(out, "{b:02x}");
        }
        out
    }

    /// The role's RPC methods.
    #[must_use]
    pub fn router(&self) -> Router {
        let mut r = Router::validated(methods::validate);
        let me = self.clone();
        r.serve_later::<methods::RegisterAccount, _, _>(move |_, req| {
            let me = me.clone();
            async move { me.register(&req).await }
        });
        let me = self.clone();
        r.serve_later::<methods::LoginAccount, _, _>(move |_, req| {
            let me = me.clone();
            async move { me.login(&req).await }
        });
        let me = self.clone();
        r.serve_later::<methods::ChangeAccountPassword, _, _>(move |_, req| {
            let me = me.clone();
            async move { me.change_password(&req).await }
        });
        let me = self.clone();
        r.serve::<methods::VerifySession>(move |_, req| me.verify_token(&req));
        let me = self.clone();
        r.serve_later::<methods::BanAccount, _, _>(move |_, req| {
            let me = me.clone();
            async move { me.ban(&req).await }
        });
        let me = self.clone();
        r.serve::<methods::Maintenance>(move |_: Role, req| {
            let was = std::mem::replace(&mut me.lock().maintenance, req.on);
            Ok(m::MaintenanceWas { was })
        });
        r
    }
}
