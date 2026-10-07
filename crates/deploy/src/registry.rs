//! The static service registry: which instance of which role listens
//! where, signed so a tampered registry is refused.
//!
//! ```toml
//! format = "mantis.registry"
//! version = 3            # the format version this build reads
//! serial = 7             # raised with every registry published
//! cluster = "dev"
//! live_key = "<64 hex>"  # Ops's live-data public key; cells verify live changes with it
//! ca = ["<hex DER>"]     # the cluster CA(s) every node's TLS trusts (two while rotating)
//!
//! [instance.persist-1]
//! role = "persist"
//! rpc = "10.40.0.14:7504"     # where callers dial its RPC
//! health = "10.40.0.14:7604"  # its health endpoint
//!
//! [instance.cells-a]
//! role = "cell-host"
//! rpc = "10.40.0.20:7520"     # the inspector Ops calls
//! health = "10.40.0.20:7620"
//! cells = [1, 2, 3]           # the cells it hosts
//!
//! [instance.social-2]         # account, realm, social, matchmaking and ops may list
//! role = "social"             # several instances: one active, the rest standbys
//! rpc = "10.40.0.15:7503"
//! health = "10.40.0.15:7603"
//! lease_owner = "social-b"    # optional: the name it holds the lease under (default: the instance name)
//!
//! [instance.gateway-1]        # the game's front door: any number of them
//! role = "gateway"
//! game = "203.0.113.7:7400"   # in place of rpc: the address clients dial (QUIC, UDP)
//! health = "10.40.0.30:7630"
//!
//! [signature]
//! algorithm = "ed25519"
//! value = "<128 hex>"
//! ```
//!
//! The signature is Ed25519 (ring, the scheme Ops signs live data with)
//! by the deploy key, over a domain tag followed by **every byte of the
//! file before the `[signature]` line**: a changed address, comment, or
//! line ending is refused. It is checked before the body is parsed. A node
//! trusts one deploy public key, from a file; the registry in turn carries
//! the live-data public key, so cells learn it from a signed source.
//!
//! Addresses are `host:port`, where the host is an IP literal or a DNS
//! name ([`Target`]). A name is resolved each time a connection is made,
//! so an instance that moves behind its name is followed; an instance that
//! moves to another name or port is followed when a registry with a higher
//! serial says so (nodes reload it without restarting, [`crate::source`]).

use std::collections::BTreeSet;
use std::fmt::Write as _;

use mantis_core::module::toml;
use mantis_services::host::Role;
use ring::signature::{ED25519, Ed25519KeyPair, UnparsedPublicKey};

use crate::fields::{FieldError, Fields};
use crate::keys::{PUBLIC_KEY_BYTES, hex, unhex};
use crate::matrix;
use crate::target::Target;

/// The `format` value.
pub const FORMAT: &str = "mantis.registry";

/// The registry format version this build reads (2: the cluster CAs; 3:
/// addresses may be DNS names).
pub const VERSION: i64 = 3;

/// The most CAs a registry carries: the current one and, while rotating,
/// the next.
pub const MAX_CAS: usize = 2;

/// Domain separation for registry signatures.
const DOMAIN: &[u8] = b"mantis.registry.v1\0";

/// The line that starts the signature.
const SIGNATURE_LINE: &str = "[signature]";

/// Why a registry is refused.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum RegistryError {
    /// No `[signature]` table, or more than one.
    Unsigned,
    /// The signature does not verify with the deploy key: the registry was
    /// changed after signing, or signed by another key.
    BadSignature,
    /// The registry is signed but malformed or inconsistent.
    Invalid(String),
}

impl std::fmt::Display for RegistryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unsigned => f.write_str("the registry is not signed (one [signature] table, last)"),
            Self::BadSignature => f.write_str(
                "the registry signature does not verify with the deploy key: \
                 refused (changed after signing, or signed by another key)",
            ),
            Self::Invalid(why) => write!(f, "the registry is invalid: {why}"),
        }
    }
}

impl std::error::Error for RegistryError {}

impl From<FieldError> for RegistryError {
    fn from(e: FieldError) -> Self {
        Self::Invalid(e.0)
    }
}

/// One process in the registry.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Instance {
    /// Its unique name (`[a-z0-9-]`, 1 to 32 characters).
    pub name: String,
    /// Its role.
    pub role: Role,
    /// Where its RPC listens (a cell host: its inspector). A gateway
    /// serves no RPC: this is its client-facing game listener (QUIC over
    /// UDP), keyed `game` in the registry ([`listener_key`]).
    pub rpc: Target,
    /// Where its health endpoint listens.
    pub health: Target,
    /// The cells it hosts (cell hosts only).
    pub cells: Vec<u64>,
    /// Its lease owner, when its role runs active and standby instances:
    /// unique in its role and the same across its restarts. `None`: the
    /// instance name.
    pub lease_owner: Option<String>,
}

impl Instance {
    /// The owner name this instance holds its role's lease under.
    #[must_use]
    pub fn owner(&self) -> &str {
        self.lease_owner.as_deref().unwrap_or(&self.name)
    }
}

/// The registry key of an instance's listener: `game` for a gateway (the
/// address clients dial), `rpc` for every other role.
#[must_use]
pub const fn listener_key(role: Role) -> &'static str {
    match role {
        Role::Gateway => "game",
        _ => "rpc",
    }
}

/// The roles that run one active instance and any number of standbys
/// (`mantis_services::failover`); the persistence writer is one instance.
pub const FAILOVER: [Role; 5] = [
    Role::Account,
    Role::Realm,
    Role::Social,
    Role::Matchmaking,
    Role::Ops,
];

/// A verified registry.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Registry {
    /// Raised with every registry published.
    pub serial: u64,
    /// The cluster's name.
    pub cluster: String,
    /// Ops's live-data public key.
    pub live_key: [u8; PUBLIC_KEY_BYTES],
    /// The cluster CA certificates (DER) every node's TLS trusts.
    pub cas: Vec<Vec<u8>>,
    /// Every instance, in file order.
    pub instances: Vec<Instance>,
}

fn valid_name(s: &str) -> bool {
    (1..=32).contains(&s.len())
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// Splits signed text into its body (every byte before the signature line)
/// and the signature table's text.
fn split(text: &str) -> Result<(&str, &str), RegistryError> {
    let mut at = None;
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        if line.trim_end_matches(['\r', '\n']) == SIGNATURE_LINE {
            if at.is_some() {
                return Err(RegistryError::Unsigned);
            }
            at = Some(offset);
        }
        offset += line.len();
    }
    let at = at.ok_or(RegistryError::Unsigned)?;
    Ok((text.get(..at).unwrap_or(""), text.get(at..).unwrap_or("")))
}

fn signed_message(body: &str) -> Vec<u8> {
    let mut msg = Vec::with_capacity(DOMAIN.len() + body.len());
    msg.extend_from_slice(DOMAIN);
    msg.extend_from_slice(body.as_bytes());
    msg
}

/// Appends a `[signature]` table signing `body` with `key`.
#[must_use]
pub fn sign(body: &str, key: &Ed25519KeyPair) -> String {
    let mut out = body.to_owned();
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    let sig = key.sign(&signed_message(&out));
    let _ = write!(
        out,
        "{SIGNATURE_LINE}\nalgorithm = \"ed25519\"\nvalue = \"{}\"\n",
        hex(sig.as_ref())
    );
    out
}

/// One `[instance.<name>]` table.
fn instance(name: &str, t: &toml::Table) -> Result<Instance, RegistryError> {
    let mut f = Fields::new("registry", t);
    let role_name = f.str("role")?;
    let role = matrix::parse(role_name).ok_or_else(|| {
        f.error(
            "role",
            "one of account, realm, social, matchmaking, persist, ops, gateway, cell-host",
        )
    })?;
    let rpc = f.target(listener_key(role))?;
    let health = f.target("health")?;
    let cells = if role == Role::Cell {
        let cells = f.uints("cells")?;
        if cells.is_empty() || cells.contains(&0) {
            return Err(f
                .error("cells", "a cell host hosts at least one cell; ids from 1")
                .into());
        }
        cells
    } else {
        Vec::new()
    };
    let lease_owner = if FAILOVER.contains(&role) {
        match f.opt_str("lease_owner")? {
            Some(o) if valid_name(o) => Some(o.to_owned()),
            Some(_) => return Err(f.error("lease_owner", "1 to 32 of a-z, 0-9 and -").into()),
            None => None,
        }
    } else {
        None
    };
    f.finish()?;
    Ok(Instance {
        name: name.to_owned(),
        role,
        rpc,
        health,
        cells,
        lease_owner,
    })
}

impl Registry {
    /// Verifies `text` with the deploy public key, then parses it.
    ///
    /// # Errors
    /// [`RegistryError`]: unsigned, a bad signature (checked before the
    /// body is read), or a malformed or inconsistent body.
    pub fn verify(text: &str, deploy_key: &[u8; PUBLIC_KEY_BYTES]) -> Result<Self, RegistryError> {
        let (body, signature) = split(text)?;
        let sig = toml::parse(signature).map_err(|e| RegistryError::Invalid(format!("signature: {e}")))?;
        let table = match sig.tables.as_slice() {
            [root, t] if root.entries.is_empty() && t.name == "signature" => t,
            _ => return Err(RegistryError::Invalid("[signature] is the last table".to_owned())),
        };
        let mut f = Fields::new("registry", table);
        if f.str("algorithm")? != "ed25519" {
            return Err(RegistryError::Invalid(
                "the signature algorithm is ed25519".to_owned(),
            ));
        }
        let value = unhex(f.str("value")?).map_err(|e| RegistryError::Invalid(format!("signature: {e}")))?;
        f.finish()?;
        UnparsedPublicKey::new(&ED25519, deploy_key)
            .verify(&signed_message(body), &value)
            .map_err(|_| RegistryError::BadSignature)?;
        Self::parse_body(body)
    }

    /// Parses an unsigned body (tooling: before signing). Nodes only ever
    /// use [`Registry::verify`].
    ///
    /// # Errors
    /// [`RegistryError::Invalid`].
    pub fn parse_body(body: &str) -> Result<Self, RegistryError> {
        let doc = toml::parse(body).map_err(|e| RegistryError::Invalid(e.to_string()))?;
        let mut tables = doc.tables.iter();
        let root = tables
            .next()
            .ok_or_else(|| RegistryError::Invalid("empty".to_owned()))?;
        let mut f = Fields::new("registry", root);
        if f.str("format")? != FORMAT {
            return Err(f.error("format", &format!("expected {FORMAT:?}")).into());
        }
        let version = f.int("version", 1..=i64::MAX)?;
        if version != VERSION {
            return Err(f
                .error(
                    "version",
                    &format!("this build reads registry format {VERSION}, not {version}"),
                )
                .into());
        }
        let serial = f.uint("serial", 1..=u64::MAX >> 1)?;
        let cluster = f.str("cluster")?.to_owned();
        if !valid_name(&cluster) {
            return Err(f.error("cluster", "names are 1 to 32 of a-z, 0-9 and -").into());
        }
        let live = unhex(f.str("live_key")?).map_err(|e| RegistryError::Invalid(format!("live_key: {e}")))?;
        let live_key: [u8; PUBLIC_KEY_BYTES] = live
            .try_into()
            .map_err(|_| RegistryError::Invalid("live_key: an Ed25519 public key is 32 bytes".to_owned()))?;
        let cas = f
            .strs("ca")?
            .iter()
            .map(|h| unhex(h).map_err(|e| RegistryError::Invalid(format!("ca: {e}"))))
            .collect::<Result<Vec<Vec<u8>>, _>>()?;
        if !(1..=MAX_CAS).contains(&cas.len()) || cas.iter().any(Vec::is_empty) {
            return Err(RegistryError::Invalid(format!(
                "ca: 1 to {MAX_CAS} CA certificates (DER, hex)"
            )));
        }
        f.finish()?;
        let mut instances = Vec::new();
        for t in tables {
            let Some(name) = t.name.strip_prefix("instance.") else {
                return Err(RegistryError::Invalid(format!(
                    "unknown table [{}] (only [instance.<name>] and [signature])",
                    t.name
                )));
            };
            if !valid_name(name) {
                return Err(RegistryError::Invalid(format!(
                    "[{}]: instance names are 1 to 32 of a-z, 0-9 and -",
                    t.name
                )));
            }
            instances.push(instance(name, t)?);
        }
        let registry = Self {
            serial,
            cluster,
            live_key,
            cas,
            instances,
        };
        registry.check()?;
        Ok(registry)
    }

    fn check(&self) -> Result<(), RegistryError> {
        let invalid = |s: String| Err(RegistryError::Invalid(s));
        let mut addrs = BTreeSet::new();
        let mut cells = BTreeSet::new();
        for i in &self.instances {
            for a in [&i.rpc, &i.health] {
                if !addrs.insert(a.clone()) {
                    return invalid(format!("{a} is listed twice ({})", i.name));
                }
            }
            for c in &i.cells {
                if !cells.insert(*c) {
                    return invalid(format!("cell {c} is hosted twice ({})", i.name));
                }
            }
        }
        if self.of(Role::Persist).len() > 1 {
            return invalid(format!(
                "persist has {} instances; the persistence writer is one instance",
                self.of(Role::Persist).len()
            ));
        }
        for role in FAILOVER {
            let mut owners = BTreeSet::new();
            for i in self.of(role) {
                if !owners.insert(i.owner()) {
                    return invalid(format!(
                        "{} instances share the lease owner {:?}; each holds the lease under its own",
                        matrix::name(role),
                        i.owner()
                    ));
                }
            }
        }
        Ok(())
    }

    /// The instances of `role`.
    #[must_use]
    pub fn of(&self, role: Role) -> Vec<&Instance> {
        self.instances.iter().filter(|i| i.role == role).collect()
    }

    /// The single instance of a service role.
    ///
    /// # Errors
    /// The registry has none.
    pub fn one(&self, role: Role) -> Result<&Instance, String> {
        self.of(role)
            .into_iter()
            .next()
            .ok_or_else(|| format!("the registry has no {} instance", matrix::name(role)))
    }

    /// The instance called `name`.
    #[must_use]
    pub fn instance(&self, name: &str) -> Option<&Instance> {
        self.instances.iter().find(|i| i.name == name)
    }

    /// The registry as text, unsigned (tooling signs it with [`sign`]).
    #[must_use]
    pub fn render(&self) -> String {
        let mut out = String::from("# The Mantis service registry. Signed: any change below is refused.\n");
        let _ = writeln!(out, "format = \"{FORMAT}\"");
        let _ = writeln!(out, "version = {VERSION}");
        let _ = writeln!(out, "serial = {}", self.serial);
        let _ = writeln!(out, "cluster = \"{}\"", self.cluster);
        let _ = writeln!(out, "live_key = \"{}\"", hex(&self.live_key));
        let cas: Vec<String> = self.cas.iter().map(|c| format!("\"{}\"", hex(c))).collect();
        let _ = writeln!(out, "ca = [{}]", cas.join(", "));
        for i in &self.instances {
            let _ = writeln!(out, "\n[instance.{}]", i.name);
            let _ = writeln!(out, "role = \"{}\"", matrix::name(i.role));
            let _ = writeln!(out, "{} = \"{}\"", listener_key(i.role), i.rpc);
            let _ = writeln!(out, "health = \"{}\"", i.health);
            if i.role == Role::Cell {
                let cells: Vec<String> = i.cells.iter().map(u64::to_string).collect();
                let _ = writeln!(out, "cells = [{}]", cells.join(", "));
            }
            if let Some(o) = &i.lease_owner {
                let _ = writeln!(out, "lease_owner = \"{o}\"");
            }
        }
        out
    }

    /// The registry as an operator reads it: every instance with its role
    /// and addresses.
    #[must_use]
    pub fn summary(&self) -> String {
        let mut out = format!("registry {} serial {}:\n", self.cluster, self.serial);
        for i in &self.instances {
            let _ = write!(
                out,
                "  {:<12} {:<16} {:<4} {:<21} health {}",
                matrix::name(i.role),
                i.name,
                listener_key(i.role),
                i.rpc.to_string(),
                i.health
            );
            if !i.cells.is_empty() {
                let cells: Vec<String> = i.cells.iter().map(u64::to_string).collect();
                let _ = write!(out, " cells {}", cells.join(","));
            }
            out.push('\n');
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keys::new_key_pair;

    fn sample() -> Registry {
        let at = |p: u16| Target::from(std::net::SocketAddr::from(([127, 0, 0, 1], p)));
        Registry {
            serial: 3,
            cluster: "dev".to_owned(),
            live_key: [7; 32],
            cas: vec![vec![0x30, 0x82, 1, 2]],
            instances: vec![
                Instance {
                    name: "persist-1".to_owned(),
                    role: Role::Persist,
                    rpc: at(7504),
                    health: at(7604),
                    cells: Vec::new(),
                    lease_owner: None,
                },
                Instance {
                    name: "cells-a".to_owned(),
                    role: Role::Cell,
                    rpc: at(7520),
                    health: at(7620),
                    cells: vec![1, 2],
                    lease_owner: None,
                },
                Instance {
                    name: "gateway-1".to_owned(),
                    role: Role::Gateway,
                    rpc: at(7400),
                    health: at(7630),
                    cells: Vec::new(),
                    lease_owner: None,
                },
            ],
        }
    }

    fn keys() -> (Ed25519KeyPair, [u8; 32]) {
        let (doc, public) = new_key_pair().unwrap();
        (Ed25519KeyPair::from_pkcs8(&doc).unwrap(), public)
    }

    #[test]
    fn a_signed_registry_round_trips() {
        let (key, public) = keys();
        let text = sign(&sample().render(), &key);
        assert_eq!(Registry::verify(&text, &public).unwrap(), sample());
        // A gateway lists the address clients dial as `game`, never `rpc`.
        let body = sample().render();
        assert!(body.contains("[instance.gateway-1]\nrole = \"gateway\"\ngame = \"127.0.0.1:7400\""));
        let as_rpc = body.replace("game = \"127.0.0.1:7400\"", "rpc = \"127.0.0.1:7400\"");
        let e = Registry::verify(&sign(&as_rpc, &key), &public).unwrap_err();
        assert!(
            matches!(&e, RegistryError::Invalid(m) if m.contains("game")),
            "{e:?}"
        );
    }

    #[test]
    fn any_changed_byte_is_refused() {
        let (key, public) = keys();
        let text = sign(&sample().render(), &key);
        let tampered = text.replace("127.0.0.1:7504", "127.0.0.1:7505");
        assert_eq!(
            Registry::verify(&tampered, &public),
            Err(RegistryError::BadSignature)
        );
        let comment = text.replacen("# The", "#  The", 1);
        assert_eq!(
            Registry::verify(&comment, &public),
            Err(RegistryError::BadSignature)
        );
        let crlf = text.replace('\n', "\r\n");
        assert_eq!(Registry::verify(&crlf, &public), Err(RegistryError::BadSignature));
        let (_, other) = keys();
        assert_eq!(Registry::verify(&text, &other), Err(RegistryError::BadSignature));
        let unsigned = sample().render();
        assert_eq!(Registry::verify(&unsigned, &public), Err(RegistryError::Unsigned));
        let twice = format!("{text}{SIGNATURE_LINE}\n");
        assert_eq!(Registry::verify(&twice, &public), Err(RegistryError::Unsigned));
        let trailing = format!("{text}extra = 1\n");
        assert!(matches!(
            Registry::verify(&trailing, &public),
            Err(RegistryError::Invalid(_))
        ));
    }

    #[test]
    fn inconsistent_registries_are_refused_even_when_signed() {
        let (key, public) = keys();
        let mut r = sample();
        r.instances[1].rpc = r.instances[0].rpc.clone();
        let e = Registry::verify(&sign(&r.render(), &key), &public).unwrap_err();
        assert!(e.to_string().contains("listed twice"), "{e}");

        let mut r = sample();
        let mut twin = r.instances[0].clone();
        twin.name = "persist-2".to_owned();
        twin.rpc = Target::parse("persist-b.internal:9000").unwrap();
        twin.health = Target::parse("persist-b.internal:9001").unwrap();
        r.instances.push(twin);
        let e = Registry::verify(&sign(&r.render(), &key), &public).unwrap_err();
        assert!(
            e.to_string().contains("the persistence writer is one instance"),
            "{e}"
        );

        // An active and a standby of a failover role: fine, with distinct
        // owners; the same owner twice is refused.
        let mut r = sample();
        let social = |name: &str, port: u16, owner: Option<&str>| Instance {
            name: name.to_owned(),
            role: Role::Social,
            rpc: Target::parse(&format!("social.internal:{port}")).unwrap(),
            health: Target::parse(&format!("social.internal:{}", port + 100)).unwrap(),
            cells: Vec::new(),
            lease_owner: owner.map(str::to_owned),
        };
        r.instances.push(social("social-1", 7503, None));
        r.instances.push(social("social-2", 7513, Some("social-standby")));
        let ok = Registry::verify(&sign(&r.render(), &key), &public).unwrap();
        assert_eq!(ok.of(Role::Social).len(), 2);
        assert_eq!(ok.instance("social-2").unwrap().owner(), "social-standby");
        r.instances[4].lease_owner = Some("social-1".to_owned());
        let e = Registry::verify(&sign(&r.render(), &key), &public).unwrap_err();
        assert!(e.to_string().contains("share the lease owner"), "{e}");

        let body = sample().render().replace("version = 3", "version = 2");
        let e = Registry::verify(&sign(&body, &key), &public).unwrap_err();
        assert!(e.to_string().contains("format 3, not 2"), "{e}");

        let body = sample().render().replace("ca = [\"30820102\"]", "ca = []");
        let e = Registry::verify(&sign(&body, &key), &public).unwrap_err();
        assert!(e.to_string().contains("1 to 2 CA"), "{e}");

        let body = sample()
            .render()
            .replace("role = \"persist\"", "role = \"persist\"\nport = 1");
        let e = Registry::verify(&sign(&body, &key), &public).unwrap_err();
        assert!(e.to_string().contains("has no key `port`"), "{e}");

        let body = format!("{}\n[routes]\n", sample().render());
        let e = Registry::verify(&sign(&body, &key), &public).unwrap_err();
        assert!(e.to_string().contains("unknown table [routes]"), "{e}");
    }
}
