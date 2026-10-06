//! Signed live changes: flags and tunables Ops changes while cells run.
//!
//! Ops signs every change with Ed25519 (ring) over a domain-separated
//! encoding of its sequence number, name, kind, and value. A cell host
//! polls the changes after the last one it applied, verifies each with the
//! cluster's live-data public key through a [`LiveFeed`], and only then
//! queues it into its cell, which applies it at the next tick boundary as
//! a logged intent. A change that fails verification, or arrives out of
//! order, stops the feed: nothing after it is applied.

use ring::rand::SecureRandom;
use ring::signature::{ED25519, Ed25519KeyPair, KeyPair, UnparsedPublicKey};

use mantis_core::wire::{BoundedArray, WireString};

use crate::generated::services as m;

/// Domain separation for live-change signatures.
const DOMAIN: &[u8] = b"mantis.live.v1\0";

/// A live change kind: a flag (a module key or `<module key>.<flag>`).
pub const FLAG: u8 = 0;
/// A live change kind: a tunable.
pub const TUNABLE: u8 = 1;

/// The bytes a live change's signature covers.
#[must_use]
pub fn signed_bytes(seq: u64, name: &str, kind: u8, value: f32) -> Vec<u8> {
    let mut out = Vec::with_capacity(DOMAIN.len() + 16 + name.len());
    out.extend_from_slice(DOMAIN);
    out.extend_from_slice(&seq.to_le_bytes());
    out.push(kind);
    out.extend_from_slice(&value.to_bits().to_le_bytes());
    out.extend_from_slice(&u32::try_from(name.len()).unwrap_or(u32::MAX).to_le_bytes());
    out.extend_from_slice(name.as_bytes());
    out
}

/// Ops's signing key for live data.
pub struct LiveSigner {
    pair: Ed25519KeyPair,
}

impl LiveSigner {
    /// A new key; also returns its PKCS#8 document, for Ops to keep.
    ///
    /// # Errors
    /// No randomness, or ring refused the key.
    pub fn generate(rng: &dyn SecureRandom) -> Result<(Self, Vec<u8>), String> {
        let doc =
            Ed25519KeyPair::generate_pkcs8(rng).map_err(|_| "cannot generate a live-data key".to_owned())?;
        let pkcs8 = doc.as_ref().to_vec();
        Ok((Self::from_pkcs8(&pkcs8)?, pkcs8))
    }

    /// The key in a PKCS#8 document.
    ///
    /// # Errors
    /// The document is not an Ed25519 key.
    pub fn from_pkcs8(pkcs8: &[u8]) -> Result<Self, String> {
        Ed25519KeyPair::from_pkcs8(pkcs8)
            .map(|pair| Self { pair })
            .map_err(|_| "not an Ed25519 PKCS#8 key".to_owned())
    }

    /// The public key cells verify with (32 bytes).
    #[must_use]
    pub fn public_key(&self) -> Vec<u8> {
        self.pair.public_key().as_ref().to_vec()
    }

    /// A signed change.
    ///
    /// # Errors
    /// The name is longer than 96 bytes, or the value is not finite.
    pub fn sign(&self, seq: u64, name: &str, kind: u8, value: f32) -> Result<m::LiveChange, String> {
        if !value.is_finite() {
            return Err("live values are finite".to_owned());
        }
        let wire_name = WireString::new(name).ok_or("live names are at most 96 bytes")?;
        let sig = self.pair.sign(&signed_bytes(seq, name, kind, value));
        Ok(m::LiveChange {
            seq,
            name: wire_name,
            kind,
            value,
            signature: BoundedArray::from_slice(sig.as_ref()).unwrap_or_default(),
        })
    }
}

/// True when `change` carries a valid signature by `public_key`.
#[must_use]
pub fn verify(public_key: &[u8], change: &m::LiveChange) -> bool {
    let sig: Vec<u8> = change.signature.iter().copied().collect();
    let bytes = signed_bytes(change.seq, change.name.as_str(), change.kind, change.value);
    UnparsedPublicKey::new(&ED25519, public_key)
        .verify(&bytes, &sig)
        .is_ok()
}

/// Why a feed stopped.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum LiveRefused {
    /// A signature did not verify.
    BadSignature {
        /// The change's sequence number.
        seq: u64,
    },
    /// A change skipped a sequence number.
    Gap {
        /// The sequence expected.
        expected: u64,
        /// The sequence received.
        got: u64,
    },
}

impl std::fmt::Display for LiveRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BadSignature { seq } => write!(f, "live change {seq} is not signed by Ops"),
            Self::Gap { expected, got } => write!(f, "live change {got} arrived before {expected}"),
        }
    }
}

impl std::error::Error for LiveRefused {}

/// One verified change, ready to queue into a cell.
#[derive(Clone, PartialEq, Debug)]
pub struct Verified {
    /// Its sequence number.
    pub seq: u64,
    /// The flag or tunable.
    pub name: String,
    /// [`FLAG`] or [`TUNABLE`].
    pub kind: u8,
    /// The value.
    pub value: f32,
}

/// A cell host's view of the live feed: what it applied, and the key it
/// trusts.
#[derive(Clone, Debug)]
pub struct LiveFeed {
    public_key: Vec<u8>,
    applied: u64,
}

impl LiveFeed {
    /// A feed that trusts `public_key` and has applied nothing.
    #[must_use]
    pub fn new(public_key: Vec<u8>) -> Self {
        Self {
            public_key,
            applied: 0,
        }
    }

    /// The last sequence number applied (the next poll's `since`).
    #[must_use]
    pub fn since(&self) -> u64 {
        self.applied
    }

    /// Verifies a poll's changes, in order. Changes already applied are
    /// skipped; the verified ones are returned and marked applied.
    ///
    /// # Errors
    /// [`LiveRefused`] at the first bad change; the ones before it are
    /// still returned through `out` and marked applied.
    pub fn accept(&mut self, changes: &m::LiveChanges, out: &mut Vec<Verified>) -> Result<(), LiveRefused> {
        for c in changes.changes.iter() {
            if c.seq <= self.applied {
                continue;
            }
            if c.seq != self.applied + 1 {
                return Err(LiveRefused::Gap {
                    expected: self.applied + 1,
                    got: c.seq,
                });
            }
            if !verify(&self.public_key, c) {
                return Err(LiveRefused::BadSignature { seq: c.seq });
            }
            self.applied = c.seq;
            out.push(Verified {
                seq: c.seq,
                name: c.name.as_str().to_owned(),
                kind: c.kind,
                value: c.value,
            });
        }
        Ok(())
    }
}
