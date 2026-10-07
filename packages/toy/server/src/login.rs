//! Logged-in load bots: the gateway's login flow for headless clients, so
//! `toy-server bots --login` can drive a cluster that verifies game tokens
//! (`--verify-tokens`, and every process-per-role deployment).
//!
//! For each bot: register an account (an existing one is reused), log in,
//! take the account's first character or create one, and select it; the
//! realm answers with the cell's game address and a single-use entry token
//! the bot presents in its handshake. The calls go to the account and realm
//! roles as the `gateway` role, proving the cluster key, over mutual TLS
//! with a gateway certificate when the cluster runs it.

use std::net::SocketAddr;
use std::sync::Arc;

use mantis_core::wire::WireString;
use mantis_services::generated::services as m;
use mantis_services::host::rpc::{RpcClient, RpcError};
use mantis_services::host::{RPC_TIMEOUT, Role};
use mantis_services::methods;
use mantis_services::tls::TlsIdentity;

/// The toy's one character kind.
pub const TOY_KIND: u32 = 1;

/// Where the gateway flow goes: the account and realm roles, and the
/// cluster key. Written by `toy-server cluster --login-out`, read by
/// `toy-server bots --login`, as `key = value` lines.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct LoginTargets {
    /// The account role's RPC address.
    pub account: SocketAddr,
    /// The realm role's RPC address.
    pub realm: SocketAddr,
    /// The cluster key.
    pub key: Vec<u8>,
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().fold(String::new(), |mut s, b| {
        use std::fmt::Write as _;
        let _ = write!(s, "{b:02x}");
        s
    })
}

fn unhex(text: &str) -> Result<Vec<u8>, String> {
    if !text.len().is_multiple_of(2) || !text.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("cluster_key: not hex".to_owned());
    }
    (0..text.len())
        .step_by(2)
        .map(|i| {
            text.get(i..i + 2)
                .and_then(|d| u8::from_str_radix(d, 16).ok())
                .ok_or_else(|| "cluster_key: not hex".to_owned())
        })
        .collect()
}

impl LoginTargets {
    /// The file's text.
    #[must_use]
    pub fn render(&self) -> String {
        format!(
            "account = {}\nrealm = {}\ncluster_key = {}\n",
            self.account,
            self.realm,
            hex(&self.key)
        )
    }

    /// Reads [`LoginTargets::render`]'s format: exactly the three keys,
    /// once each; blank lines and `#` comments are allowed.
    ///
    /// # Errors
    /// A missing, unknown or repeated key, or a bad value.
    pub fn parse(text: &str) -> Result<Self, String> {
        let (mut account, mut realm, mut key) = (None, None, None);
        for (n, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let (k, v) = line
                .split_once('=')
                .map(|(k, v)| (k.trim(), v.trim()))
                .ok_or_else(|| format!("line {}: expected key = value", n + 1))?;
            let addr = |v: &str| {
                v.parse::<SocketAddr>()
                    .map_err(|_| format!("{k}: not an address"))
            };
            let slot_taken = match k {
                "account" => account.replace(addr(v)?).is_some(),
                "realm" => realm.replace(addr(v)?).is_some(),
                "cluster_key" => key.replace(unhex(v)?).is_some(),
                other => return Err(format!("line {}: unknown key {other}", n + 1)),
            };
            if slot_taken {
                return Err(format!("{k} given twice"));
            }
        }
        let key: Vec<u8> = key.ok_or("cluster_key is missing")?;
        if key.is_empty() {
            return Err("cluster_key is empty".to_owned());
        }
        Ok(Self {
            account: account.ok_or("account is missing")?,
            realm: realm.ok_or("realm is missing")?,
            key,
        })
    }
}

/// Where a logged-in bot goes.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Entry {
    /// Its account.
    pub account: u64,
    /// Its character.
    pub character: u64,
    /// The cell it was placed in.
    pub cell: u64,
    /// That cell's game address.
    pub address: String,
    /// The single-use entry token for the handshake.
    pub token: Vec<u8>,
}

/// The gateway's clients of the account and realm roles.
pub struct Gateway {
    account: RpcClient,
    realm: RpcClient,
}

impl Gateway {
    /// Clients of `targets`, over mutual TLS with a gateway identity when
    /// `tls` is given.
    ///
    /// # Errors
    /// The identity is not a gateway's, or does not make a configuration.
    pub fn new(targets: &LoginTargets, tls: Option<&Arc<TlsIdentity>>) -> Result<Self, String> {
        let client = |addr, server| {
            RpcClient::with_tls(addr, Role::Gateway, targets.key.clone(), tls.cloned(), server)
                .map_err(|e| e.to_string())
        };
        Ok(Self {
            account: client(targets.account, Role::Account)?,
            realm: client(targets.realm, Role::Realm)?,
        })
    }

    /// Registers `name` (or reuses it), logs in, and selects its first
    /// character (creating one): where to go, and the token to show.
    ///
    /// # Errors
    /// Which step failed and why (a wrong password for an existing name, a
    /// role unreachable, the realm without a cell).
    pub async fn enter(&self, name: &str, password: &str) -> Result<Entry, String> {
        let name_w = WireString::<32>::new(name).ok_or("the name is longer than 32 bytes")?;
        let password_w = WireString::<64>::new(password).ok_or("the password is longer than 64 bytes")?;
        let step = |what: &'static str| move |e: RpcError| format!("{name}: {what}: {e}");
        let register = m::Register {
            name: name_w,
            password: password_w,
        };
        match self
            .account
            .call::<methods::RegisterAccount>(&register, RPC_TIMEOUT)
            .await
        {
            Ok(_) | Err(RpcError::Refused(_)) => {}
            Err(e) => return Err(step("register")(e)),
        }
        let login = m::Login {
            name: name_w,
            password: password_w,
        };
        let session = self
            .account
            .call::<methods::LoginAccount>(&login, RPC_TIMEOUT)
            .await
            .map_err(step("log in"))?;
        let account = session.account;
        let listed = self
            .realm
            .call::<methods::ListAccountCharacters>(&m::ListCharacters { account }, RPC_TIMEOUT)
            .await
            .map_err(step("list characters"))?;
        let character = match listed.ids.iter().next() {
            Some(c) => *c,
            None => {
                self.realm
                    .call::<methods::NewCharacter>(
                        &m::CreateCharacter {
                            account,
                            name: name_w,
                            kind: TOY_KIND,
                        },
                        RPC_TIMEOUT,
                    )
                    .await
                    .map_err(step("create a character"))?
                    .character
            }
        };
        let placed = self
            .realm
            .call::<methods::Select>(&m::SelectCharacter { account, character }, RPC_TIMEOUT)
            .await
            .map_err(step("select"))?;
        Ok(Entry {
            account: account.0,
            character: character.0,
            cell: placed.cell.0,
            address: placed.address.as_str().to_owned(),
            token: placed.token.iter().copied().collect(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn login_targets_round_trip_and_refuse_anything_else() {
        let t = LoginTargets {
            account: "127.0.0.1:5001"
                .parse()
                .unwrap_or_else(|_| SocketAddr::from(([0; 4], 0))),
            realm: "127.0.0.1:5002"
                .parse()
                .unwrap_or_else(|_| SocketAddr::from(([0; 4], 0))),
            key: vec![0, 1, 0xab, 0xff],
        };
        assert_eq!(LoginTargets::parse(&t.render()), Ok(t.clone()));
        let with_comment = format!("# from toy-server cluster\n\n{}", t.render());
        assert_eq!(LoginTargets::parse(&with_comment), Ok(t.clone()));
        for bad in [
            String::new(),
            t.render().replace("realm", "realms"),
            format!("{}realm = 127.0.0.1:1\n", t.render()),
            t.render().replace("0001abff", "0001abf"),
            t.render().replace("0001abff", "zz"),
            t.render().replace("127.0.0.1:5001", "nowhere"),
            "account = 127.0.0.1:1\nrealm = 127.0.0.1:2\ncluster_key = \n".to_owned(),
        ] {
            assert!(LoginTargets::parse(&bad).is_err(), "{bad:?}");
        }
    }
}
