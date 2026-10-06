//! Account: authentication, session tokens, bans, maintenance mode.
//!
//! Passwords are stretched with PBKDF2-HMAC-SHA256 and a per-account random
//! salt; only the derived key is kept. A session token is 32 random bytes,
//! valid for five minutes and for one verification (a cell consumes it at
//! the handshake).

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::num::NonZeroU32;
use std::sync::{Arc, Mutex, MutexGuard};

use mantis_core::wire::BoundedArray;
use ring::rand::SecureRandom;
use ring::{digest, pbkdf2};

use crate::generated::services as m;
use crate::host::Role;
use crate::host::clock::ServiceClock;
use crate::host::refused;
use crate::host::rpc::{Router, RpcError};
use crate::methods;

/// How long a session token stays valid, in milliseconds.
pub const TOKEN_MS: u64 = 5 * 60 * 1000;

const ITERATIONS: u32 = 10_000;

struct Account {
    id: u64,
    salt: [u8; 16],
    key: [u8; 32],
    banned_until_ms: u64,
}

#[derive(Default)]
struct State {
    next: u64,
    by_name: BTreeMap<String, Account>,
    tokens: BTreeMap<[u8; 32], (u64, u64)>,
    maintenance: bool,
}

/// The account role.
#[derive(Clone, Default)]
pub struct AccountService {
    state: Arc<Mutex<State>>,
    rng: Arc<crate::host::Random>,
    clock: ServiceClock,
}

fn derive(salt: &[u8], password: &str) -> [u8; 32] {
    let mut key = [0u8; 32];
    let iterations = NonZeroU32::new(ITERATIONS).unwrap_or(NonZeroU32::MIN);
    pbkdf2::derive(
        pbkdf2::PBKDF2_HMAC_SHA256,
        iterations,
        salt,
        password.as_bytes(),
        &mut key,
    );
    key
}

impl AccountService {
    /// An empty account store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The same role on `clock` (token lifetimes and bans).
    #[must_use]
    pub fn clocked(mut self, clock: ServiceClock) -> Self {
        self.clock = clock;
        self
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        crate::host::lock(&self.state)
    }

    fn register(&self, req: &m::Register) -> Result<m::Registered, RpcError> {
        let mut salt = [0u8; 16];
        self.rng.0.fill(&mut salt).map_err(|_| refused("no randomness"))?;
        let key = derive(&salt, req.password.as_str());
        let mut s = self.lock();
        if s.by_name.contains_key(req.name.as_str()) {
            return Err(refused("name taken"));
        }
        s.next += 1;
        let id = s.next;
        s.by_name.insert(
            req.name.as_str().to_owned(),
            Account {
                id,
                salt,
                key,
                banned_until_ms: 0,
            },
        );
        Ok(m::Registered {
            account: m::AccountId(id),
        })
    }

    fn login(&self, req: &m::Login) -> Result<m::Session, RpcError> {
        let (id, salt, key, banned) = {
            let s = self.lock();
            if s.maintenance {
                return Err(refused("maintenance"));
            }
            let a = s
                .by_name
                .get(req.name.as_str())
                .ok_or_else(|| refused("bad credentials"))?;
            (a.id, a.salt, a.key, a.banned_until_ms)
        };
        let ok = pbkdf2::verify(
            pbkdf2::PBKDF2_HMAC_SHA256,
            NonZeroU32::new(ITERATIONS).unwrap_or(NonZeroU32::MIN),
            &salt,
            req.password.as_str().as_bytes(),
            &key,
        )
        .is_ok();
        if !ok {
            return Err(refused("bad credentials"));
        }
        if banned > self.clock.now_ms() {
            return Err(refused("banned"));
        }
        let mut token = [0u8; 32];
        self.rng
            .0
            .fill(&mut token)
            .map_err(|_| refused("no randomness"))?;
        let expires = self.clock.now_ms() + TOKEN_MS;
        self.lock().tokens.insert(token, (id, expires));
        Ok(m::Session {
            account: m::AccountId(id),
            token: BoundedArray::from_slice(&token).unwrap_or_default(),
        })
    }

    fn verify(&self, req: &m::VerifyToken) -> Result<m::Verified, RpcError> {
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

    fn ban(&self, req: &m::Ban) -> Result<m::Banned, RpcError> {
        let mut s = self.lock();
        let a = s
            .by_name
            .values_mut()
            .find(|a| a.id == req.account.0)
            .ok_or_else(|| refused("no such account"))?;
        let previous_until_ms = std::mem::replace(&mut a.banned_until_ms, req.until_ms);
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
        self.lock()
            .by_name
            .values()
            .find(|a| a.id == account)
            .map(|a| a.banned_until_ms)
    }

    /// A short fingerprint of the account store (Ops before/after rows).
    #[must_use]
    pub fn fingerprint(&self) -> String {
        let s = self.lock();
        let mut ctx = digest::Context::new(&digest::SHA256);
        for (name, a) in &s.by_name {
            ctx.update(name.as_bytes());
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
        r.serve::<methods::RegisterAccount>(move |_, req| me.register(&req));
        let me = self.clone();
        r.serve::<methods::LoginAccount>(move |_, req| me.login(&req));
        let me = self.clone();
        r.serve::<methods::VerifySession>(move |_, req| me.verify(&req));
        let me = self.clone();
        r.serve::<methods::BanAccount>(move |_, req| me.ban(&req));
        let me = self.clone();
        r.serve::<methods::Maintenance>(move |_: Role, req| {
            let was = std::mem::replace(&mut me.lock().maintenance, req.on);
            Ok(m::MaintenanceWas { was })
        });
        r
    }
}
