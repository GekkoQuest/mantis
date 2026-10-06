//! Bundle v1: a signed, versioned manifest of cooked content (plan 11).
//!
//! A cook emits one bundle per **domain**: `gameplay` (what client and server must agree
//! on: tables, gameplay graphs, and the client copy of every sector; its hash is the one
//! the handshake compares), `server` (server-only content: the server copy of every
//! sector), and `presentation` (meshes, textures, materials, presentation graphs, UI,
//! sounds, animation; its hash is separate, so retiming an effect never changes the
//! gameplay hash). Payloads live in a content-addressed store keyed by their hash; the
//! bundle lists them by logical name.
//!
//! The manifest is signed with Ed25519. [`SignedBundle::parse`] checks structure only;
//! [`SignedBundle::verify`] checks the signature against a trusted public key. Hosts refuse a
//! bundle that fails either.
//!
//! # Layout (little-endian)
//!
//! | offset | size | field |
//! |---|---|---|
//! | 0 | 4 | magic `"MBND"` |
//! | 4 | 2 | version `u16` = 1 |
//! | 6 | 2 | flags `u16` = 0 |
//! | 8 | 1 | domain `u8`: 0 gameplay, 1 server, 2 presentation |
//! | 9 | 3 | reserved, 0 |
//! | 12 | 4 | content version `u32` (the package's, increasing per release) |
//! | 16 | 4 | entry count `u32`, at most [`MAX_ENTRIES`] |
//! | 20 | 4 | reserved, 0 |
//! | 24 | | entries, sorted by name, names unique |
//! | | 64 | Ed25519 signature over every byte before it |
//!
//! Entry: name length `u16` (1 to [`MAX_NAME`]), name bytes (UTF-8, `/`-separated
//! relative path), kind `u16` ([`AssetKind`] code), reserved `u16` = 0, size `u64`,
//! content hash `[u8; 32]` (BLAKE3 of the payload).

use mantis_core::content::ContentHash;

use crate::bytes::{FormatError, Reader, Writer};

/// Magic bytes.
pub const MAGIC: [u8; 4] = *b"MBND";
/// Most entries in one bundle.
pub const MAX_ENTRIES: usize = 1 << 20;
/// Longest entry name, bytes.
pub const MAX_NAME: usize = 512;
/// Signature length.
pub const SIGNATURE_LEN: usize = 64;

/// Which content a bundle carries.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)]
pub enum Domain {
    /// Shared by client and server; its hash is checked at the handshake.
    Gameplay,
    /// Server only.
    Server,
    /// Client presentation; its own hash.
    Presentation,
}

impl Domain {
    fn code(self) -> u8 {
        match self {
            Domain::Gameplay => 0,
            Domain::Server => 1,
            Domain::Presentation => 2,
        }
    }

    fn from_code(c: u8) -> Option<Self> {
        Some(match c {
            0 => Domain::Gameplay,
            1 => Domain::Server,
            2 => Domain::Presentation,
            _ => return None,
        })
    }
}

/// What a payload is.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)]
pub enum AssetKind {
    /// A text table.
    Table,
    /// A gameplay graph.
    GameplayGraph,
    /// A sector (`crate::sector`).
    Sector,
    /// A mesh (`crate::mesh`).
    Mesh,
    /// A texture (`crate::texture`).
    Texture,
    /// A material (`crate::material`).
    Material,
    /// A presentation graph (`crate::presentation`).
    Presentation,
    /// A UI layout or theme (markup text).
    Ui,
    /// A probe volume (`crate::probe_volume`).
    ProbeVolume,
    /// A lightmap (`crate::lightmap`).
    Lightmap,
    /// A color grading (`crate::color_grading`).
    ColorGrading,
    /// A particle effect (`crate::particle_effect`).
    ParticleEffect,
    /// A skeleton (`crate::skeleton`).
    Skeleton,
    /// An animation clip (`crate::anim_clip`).
    AnimClip,
    /// An animation graph (`crate::anim_graph`).
    AnimGraph,
    /// A vertex animation bake (texels for the mid and far crowd tiers).
    VertexAnimation,
    /// A sound bank (`crate::sound_bank`).
    SoundBank,
    /// A mixer graph (`crate::mixer_graph`).
    MixerGraph,
    /// A package-defined kind (from an importer plugin).
    Package(u16),
}

impl AssetKind {
    /// The wire code (package kinds from 1000).
    pub fn code(self) -> u16 {
        match self {
            AssetKind::Table => 1,
            AssetKind::GameplayGraph => 2,
            AssetKind::Sector => 3,
            AssetKind::Mesh => 4,
            AssetKind::Texture => 5,
            AssetKind::Material => 6,
            AssetKind::Presentation => 7,
            AssetKind::Ui => 8,
            AssetKind::ProbeVolume => 9,
            AssetKind::Lightmap => 10,
            AssetKind::ColorGrading => 11,
            AssetKind::ParticleEffect => 12,
            AssetKind::Skeleton => 13,
            AssetKind::AnimClip => 14,
            AssetKind::AnimGraph => 15,
            AssetKind::VertexAnimation => 16,
            AssetKind::SoundBank => 17,
            AssetKind::MixerGraph => 18,
            AssetKind::Package(n) => n,
        }
    }

    /// The kind for a wire code.
    pub fn from_code(c: u16) -> Option<Self> {
        Some(match c {
            1 => AssetKind::Table,
            2 => AssetKind::GameplayGraph,
            3 => AssetKind::Sector,
            4 => AssetKind::Mesh,
            5 => AssetKind::Texture,
            6 => AssetKind::Material,
            7 => AssetKind::Presentation,
            8 => AssetKind::Ui,
            9 => AssetKind::ProbeVolume,
            10 => AssetKind::Lightmap,
            11 => AssetKind::ColorGrading,
            12 => AssetKind::ParticleEffect,
            13 => AssetKind::Skeleton,
            14 => AssetKind::AnimClip,
            15 => AssetKind::AnimGraph,
            16 => AssetKind::VertexAnimation,
            17 => AssetKind::SoundBank,
            18 => AssetKind::MixerGraph,
            n if n >= 1000 => AssetKind::Package(n),
            _ => return None,
        })
    }
}

/// One listed payload.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Entry {
    /// Logical name (relative path, `/`-separated).
    pub name: String,
    /// What it is.
    pub kind: AssetKind,
    /// Payload bytes.
    pub size: u64,
    /// BLAKE3 of the payload.
    pub hash: ContentHash,
}

/// A bundle manifest.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Bundle {
    /// Which content.
    pub domain: Domain,
    /// The package's content version.
    pub content_version: u32,
    /// Payloads, sorted by name.
    pub entries: Vec<Entry>,
}

/// A parsed bundle with its signature, not yet verified.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct SignedBundle {
    /// The manifest.
    pub bundle: Bundle,
    /// The signed bytes (everything before the signature).
    pub signed: Vec<u8>,
    /// The signature.
    pub signature: [u8; SIGNATURE_LEN],
}

fn valid_name(n: &str) -> bool {
    !n.is_empty()
        && n.len() <= MAX_NAME
        && !n.starts_with('/')
        && n.split('/').all(|p| !p.is_empty() && p != "." && p != "..")
        && !n.contains('\\')
}

impl Bundle {
    /// Checks names (valid relative paths, unique, sorted) and the entry count.
    ///
    /// # Errors
    /// [`FormatError::Dimensions`], [`FormatError::Encoding`] for a bad name, or
    /// [`FormatError::Inconsistent`] when names are not strictly sorted.
    pub fn validate(&self) -> Result<(), FormatError> {
        if self.entries.len() > MAX_ENTRIES {
            return Err(FormatError::Dimensions);
        }
        if self.entries.iter().any(|e| !valid_name(&e.name)) {
            return Err(FormatError::Encoding(0));
        }
        if self
            .entries
            .windows(2)
            .any(|w| matches!(w, [a, b] if a.name >= b.name))
        {
            return Err(FormatError::Inconsistent);
        }
        Ok(())
    }

    /// The bytes a signature covers.
    pub fn signed_bytes(&self) -> Vec<u8> {
        let mut w = Writer::new();
        w.bytes(&MAGIC);
        w.u16(1);
        w.u16(0);
        w.u8(self.domain.code());
        w.bytes(&[0; 3]);
        w.u32(self.content_version);
        w.count(self.entries.len());
        w.u32(0);
        for e in &self.entries {
            w.u16(u16::try_from(e.name.len()).unwrap_or(u16::MAX));
            w.bytes(e.name.as_bytes());
            w.u16(e.kind.code());
            w.u16(0);
            w.bytes(&e.size.to_le_bytes());
            w.bytes(e.hash.as_bytes());
        }
        w.into_bytes()
    }

    /// The bundle's identity: BLAKE3 of the signed bytes (the hash a handshake compares
    /// for the gameplay domain).
    pub fn hash(&self) -> ContentHash {
        ContentHash::of(&self.signed_bytes())
    }

    /// Encodes with `signature` appended (the cook signs [`Bundle::signed_bytes`]).
    pub fn encode(&self, signature: &[u8; SIGNATURE_LEN]) -> Vec<u8> {
        let mut out = self.signed_bytes();
        out.extend_from_slice(signature);
        out
    }

    /// Looks an entry up by name.
    pub fn get(&self, name: &str) -> Option<&Entry> {
        self.entries
            .binary_search_by(|e| e.name.as_str().cmp(name))
            .ok()
            .and_then(|i| self.entries.get(i))
    }
}

impl SignedBundle {
    /// Parses and validates the structure (not the signature).
    ///
    /// # Errors
    /// [`FormatError`] describing the first problem found.
    pub fn parse(bytes: &[u8]) -> Result<SignedBundle, FormatError> {
        let body_len = bytes
            .len()
            .checked_sub(SIGNATURE_LEN)
            .ok_or(FormatError::Length {
                expected: SIGNATURE_LEN as u64,
                actual: bytes.len() as u64,
            })?;
        let (body, sig) = bytes.split_at(body_len);
        let mut r = Reader::new(body);
        if r.array::<4>()? != MAGIC {
            return Err(FormatError::Magic);
        }
        let version = r.u16()?;
        if version != 1 {
            return Err(FormatError::Version(version));
        }
        let flags = r.u16()?;
        if flags != 0 {
            return Err(FormatError::Flags(u32::from(flags)));
        }
        let code = r.u8()?;
        let domain = Domain::from_code(code).ok_or(FormatError::Encoding(u32::from(code)))?;
        if r.array::<3>()? != [0; 3] {
            return Err(FormatError::Reserved);
        }
        let content_version = r.u32()?;
        let count = r.u32()? as usize;
        if count > MAX_ENTRIES {
            return Err(FormatError::Dimensions);
        }
        if r.u32()? != 0 {
            return Err(FormatError::Reserved);
        }
        let mut entries = Vec::with_capacity(count.min(r.remaining() / 48));
        for _ in 0..count {
            let len = usize::from(r.u16()?);
            if len == 0 || len > MAX_NAME {
                return Err(FormatError::Dimensions);
            }
            let name = core::str::from_utf8(r.slice(len)?)
                .map_err(|_| FormatError::Encoding(0))?
                .to_owned();
            let code = r.u16()?;
            let kind = AssetKind::from_code(code).ok_or(FormatError::Encoding(u32::from(code)))?;
            if r.u16()? != 0 {
                return Err(FormatError::Reserved);
            }
            let size = u64::from_le_bytes(r.array()?);
            let hash = ContentHash::from_bytes(r.array()?);
            entries.push(Entry {
                name,
                kind,
                size,
                hash,
            });
        }
        r.finish()?;
        let bundle = Bundle {
            domain,
            content_version,
            entries,
        };
        bundle.validate()?;
        let mut signature = [0u8; SIGNATURE_LEN];
        signature.copy_from_slice(sig);
        Ok(SignedBundle {
            bundle,
            signed: body.to_vec(),
            signature,
        })
    }

    /// Verifies the Ed25519 signature against `public_key` (32 bytes).
    ///
    /// # Errors
    /// [`FormatError::Validity`] when the signature does not verify.
    pub fn verify(&self, public_key: &[u8]) -> Result<&Bundle, FormatError> {
        let key = ring::signature::UnparsedPublicKey::new(&ring::signature::ED25519, public_key);
        key.verify(&self.signed, &self.signature)
            .map_err(|_| FormatError::Validity)?;
        Ok(&self.bundle)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    fn sample() -> Bundle {
        let e = |name: &str, kind, fill: u8| Entry {
            name: name.to_owned(),
            kind,
            size: 10,
            hash: ContentHash::from_bytes([fill; 32]),
        };
        Bundle {
            domain: Domain::Gameplay,
            content_version: 7,
            entries: vec![
                e("sectors/0_0.sector", AssetKind::Sector, 1),
                e("tables/std.vendor.stock", AssetKind::Table, 2),
                e("tables/z.custom", AssetKind::Package(1001), 3),
            ],
        }
    }

    #[test]
    fn round_trips_and_looks_up() -> TestResult {
        let b = sample();
        let bytes = b.encode(&[9; SIGNATURE_LEN]);
        let parsed = SignedBundle::parse(&bytes)?;
        assert_eq!(parsed.bundle, b);
        assert_eq!(parsed.signature, [9; SIGNATURE_LEN]);
        assert_eq!(
            parsed.bundle.get("tables/std.vendor.stock").map(|e| e.kind),
            Some(AssetKind::Table)
        );
        assert!(parsed.bundle.get("missing").is_none());
        Ok(())
    }

    #[test]
    fn rules_reject() {
        let bad = |edit: fn(&mut Bundle)| {
            let mut b = sample();
            edit(&mut b);
            SignedBundle::parse(&b.encode(&[0; SIGNATURE_LEN])).err()
        };
        assert_eq!(
            bad(|b| b.entries.swap(0, 1)),
            Some(FormatError::Inconsistent),
            "sorted"
        );
        assert_eq!(
            bad(|b| if let Some(e) = b.entries.first_mut() {
                e.name = "../escape".to_owned();
            }),
            Some(FormatError::Encoding(0))
        );
        assert!(SignedBundle::parse(&[0; 10]).is_err());
    }

    #[test]
    fn no_corruption_or_truncation_panics() {
        let bytes = sample().encode(&[1; SIGNATURE_LEN]);
        for cut in 0..bytes.len() {
            let _ = SignedBundle::parse(bytes.get(..cut).unwrap_or(&[]));
        }
        for i in 0..bytes.len() {
            let mut b = bytes.clone();
            if let Some(x) = b.get_mut(i) {
                *x ^= 0x81;
            }
            let _ = SignedBundle::parse(&b);
        }
    }
}
