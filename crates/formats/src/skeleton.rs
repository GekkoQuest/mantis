//! Skeleton v1: a bone hierarchy with bind-pose local transforms and inverse bind
//! matrices, produced by the cook and read by the animation runtime.
//!
//! Spatial convention: every position, direction, and transform in this format is in the
//! left-handed world frame of decision 0019 (+X right, +Y up, +Z forward, as
//! `mantis_core::kinematics` defines it); bone transforms are parent-local (the root model-local) in that frame.
//!
//! Layout (little-endian):
//!
//! | offset | size | field |
//! |---|---|---|
//! | 0 | 4 | magic `"MSKL"` |
//! | 4 | 2 | version `u16` = 1 |
//! | 6 | 2 | flags `u16` = 0 |
//! | 8 | 4 | bone count `u32`, 1 to [`MAX_BONES`] |
//! | 12 | 4 | reserved, 0 |
//! | 16 | | bones, [`BONE_BYTES`] each |
//!
//! Each bone (112 bytes):
//!
//! | offset | size | field |
//! |---|---|---|
//! | 0 | 2 | parent `u16`: [`NO_PARENT`] for a root, else the index of an earlier bone |
//! | 2 | 2 | reserved, 0 |
//! | 4 | 4 | name hash `u32`: [`bone_name_hash`] of the authored name, unique in the skeleton |
//! | 8 | 12 | bind translation `[f32; 3]` (local, relative to the parent) |
//! | 20 | 16 | bind rotation `[f32; 4]` quaternion `x, y, z, w`, unit length within [`UNIT_TOLERANCE`] |
//! | 36 | 12 | bind scale `[f32; 3]`, every component positive |
//! | 48 | 64 | inverse bind matrix `[f32; 16]`, column-major; the bottom row (elements 3, 7, 11, 15) is exactly `0, 0, 0, 1` |
//!
//! Parents precede children, so bone 0 is always a root and a single forward pass over
//! the bones computes model-space transforms. Every `f32` must be finite, and the length
//! must be exactly `16 + 112 * bone_count`.

use crate::bytes::{FormatError, Reader, Writer};

/// Magic bytes.
pub const MAGIC: [u8; 4] = *b"MSKL";
/// Most bones per skeleton.
pub const MAX_BONES: u32 = 1024;
/// The parent value of a root bone.
pub const NO_PARENT: u16 = 0xFFFF;
/// Bytes per bone record.
pub const BONE_BYTES: u64 = 112;
/// How far a quaternion's length may be from 1.
pub const UNIT_TOLERANCE: f32 = 1e-3;
const HEADER: u64 = 16;

/// The stable bone id: FNV-1a 32 of the authored bone name's UTF-8 bytes.
pub fn bone_name_hash(name: &str) -> u32 {
    let mut hash: u32 = 0x811C_9DC5;
    for b in name.bytes() {
        hash ^= u32::from(b);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash
}

/// True when `q` is a unit quaternion within [`UNIT_TOLERANCE`].
pub fn is_unit_quaternion(q: [f32; 4]) -> bool {
    let len_sq: f32 = q.iter().map(|c| c * c).sum();
    q.iter().all(|c| c.is_finite()) && (len_sq.sqrt() - 1.0).abs() <= UNIT_TOLERANCE
}

/// One bone.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct BoneDef {
    /// Parent bone index; `None` for a root. Always less than this bone's index.
    pub parent: Option<u16>,
    /// [`bone_name_hash`] of the authored name.
    pub name_hash: u32,
    /// Bind-pose local translation.
    pub translation: [f32; 3],
    /// Bind-pose local rotation, `x, y, z, w`.
    pub rotation: [f32; 4],
    /// Bind-pose local scale.
    pub scale: [f32; 3],
    /// Inverse bind matrix, column-major.
    pub inverse_bind: [f32; 16],
}

/// A skeleton asset.
#[derive(Clone, PartialEq, Debug)]
pub struct SkeletonAsset {
    /// Bones, parents first.
    pub bones: Vec<BoneDef>,
}

impl SkeletonAsset {
    /// Checks every rule of the format on an in-memory skeleton (the parser runs it too).
    ///
    /// # Errors
    /// [`FormatError::Dimensions`] for a bone count out of range,
    /// [`FormatError::Inconsistent`] for a parent that does not precede its child,
    /// [`FormatError::NonFinite`] for a non-finite value, [`FormatError::Geometry`] for a
    /// non-unit rotation, a non-positive scale, or a non-affine inverse bind matrix, and
    /// [`FormatError::DuplicateId`] for a repeated name hash.
    pub fn validate(&self) -> Result<(), FormatError> {
        if self.bones.is_empty() || self.bones.len() > MAX_BONES as usize {
            return Err(FormatError::Dimensions);
        }
        for (i, bone) in self.bones.iter().enumerate() {
            if let Some(p) = bone.parent
                && (p == NO_PARENT || usize::from(p) >= i)
            {
                return Err(FormatError::Inconsistent);
            }
            let mut floats = bone
                .translation
                .iter()
                .chain(&bone.rotation)
                .chain(&bone.scale)
                .chain(&bone.inverse_bind);
            if floats.any(|v| !v.is_finite()) {
                return Err(FormatError::NonFinite);
            }
            if !is_unit_quaternion(bone.rotation) || bone.scale.iter().any(|s| *s <= 0.0) {
                return Err(FormatError::Geometry);
            }
            let m = &bone.inverse_bind;
            let bottom = [m.get(3), m.get(7), m.get(11), m.get(15)];
            if bottom != [Some(&0.0), Some(&0.0), Some(&0.0), Some(&1.0)] {
                return Err(FormatError::Geometry);
            }
        }
        let mut hashes: Vec<u32> = self.bones.iter().map(|b| b.name_hash).collect();
        hashes.sort_unstable();
        if let Some(pair) = hashes.windows(2).find(|w| w.first() == w.get(1)) {
            return Err(FormatError::DuplicateId(pair.first().copied().unwrap_or(0)));
        }
        Ok(())
    }

    /// Index of the bone with `name_hash`.
    pub fn find(&self, name_hash: u32) -> Option<usize> {
        self.bones.iter().position(|b| b.name_hash == name_hash)
    }

    /// Parses and validates a skeleton.
    ///
    /// # Errors
    /// [`FormatError`] describing the first problem found.
    pub fn parse(bytes: &[u8]) -> Result<SkeletonAsset, FormatError> {
        let mut r = Reader::new(bytes);
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
        let count = r.u32()?;
        if count == 0 || count > MAX_BONES {
            return Err(FormatError::Dimensions);
        }
        if r.u32()? != 0 {
            return Err(FormatError::Reserved);
        }
        let expected = HEADER + BONE_BYTES * u64::from(count);
        if bytes.len() as u64 != expected {
            return Err(FormatError::Length {
                expected,
                actual: bytes.len() as u64,
            });
        }
        let mut bones = Vec::with_capacity(count as usize);
        for _ in 0..count {
            bones.push(read_bone(&mut r)?);
        }
        r.finish()?;
        let skeleton = SkeletonAsset { bones };
        skeleton.validate()?;
        Ok(skeleton)
    }

    /// Reference encoder (the exact inverse of [`SkeletonAsset::parse`]).
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer::new();
        w.bytes(&MAGIC);
        w.u16(1);
        w.u16(0);
        w.count(self.bones.len());
        w.u32(0);
        for bone in &self.bones {
            w.u16(bone.parent.unwrap_or(NO_PARENT));
            w.u16(0);
            w.u32(bone.name_hash);
            w.vec3(bone.translation);
            for c in bone.rotation {
                w.f32(c);
            }
            w.vec3(bone.scale);
            for c in bone.inverse_bind {
                w.f32(c);
            }
        }
        w.into_bytes()
    }
}

fn read_bone(r: &mut Reader<'_>) -> Result<BoneDef, FormatError> {
    let parent = r.u16()?;
    if r.u16()? != 0 {
        return Err(FormatError::Reserved);
    }
    let name_hash = r.u32()?;
    let translation = r.vec3()?;
    let rotation = [r.f32()?, r.f32()?, r.f32()?, r.f32()?];
    let scale = r.vec3()?;
    let mut inverse_bind = [0.0f32; 16];
    for c in &mut inverse_bind {
        *c = r.f32()?;
    }
    Ok(BoneDef {
        parent: (parent != NO_PARENT).then_some(parent),
        name_hash,
        translation,
        rotation,
        scale,
        inverse_bind,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    const IDENTITY: [f32; 16] = [
        1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0,
    ];

    fn bone(parent: Option<u16>, name: &str, y: f32) -> BoneDef {
        let mut inverse_bind = IDENTITY;
        if let Some(t) = inverse_bind.get_mut(13) {
            *t = -y;
        }
        BoneDef {
            parent,
            name_hash: bone_name_hash(name),
            translation: [0.0, y, 0.0],
            rotation: [0.0, 0.0, 0.0, 1.0],
            scale: [1.0; 3],
            inverse_bind,
        }
    }

    /// A three-bone chain: root, hip, knee.
    fn chain() -> SkeletonAsset {
        SkeletonAsset {
            bones: vec![
                bone(None, "root", 0.0),
                bone(Some(0), "hip", 1.0),
                bone(Some(1), "knee", 0.5),
            ],
        }
    }

    #[test]
    fn name_hash_is_fnv1a() {
        assert_eq!(bone_name_hash(""), 0x811C_9DC5);
        assert_eq!(bone_name_hash("a"), 0xE40C_292C);
        assert_eq!(bone_name_hash("foobar"), 0xBF9C_F968);
    }

    #[test]
    fn round_trips() -> TestResult {
        let s = chain();
        let bytes = s.encode();
        assert_eq!(bytes.len(), 16 + 112 * 3);
        let back = SkeletonAsset::parse(&bytes)?;
        assert_eq!(back, s);
        assert_eq!(back.encode(), bytes);
        assert_eq!(back.find(bone_name_hash("knee")), Some(2));
        assert_eq!(back.find(bone_name_hash("tail")), None);
        Ok(())
    }

    #[test]
    fn rejects_every_malformed_field() {
        let good = chain().encode();
        let corrupt = |at: usize, bytes: &[u8]| {
            let mut b = good.clone();
            if let Some(s) = b.get_mut(at..at + bytes.len()) {
                s.copy_from_slice(bytes);
            }
            SkeletonAsset::parse(&b)
        };
        let bone1 = 16 + 112;
        assert_eq!(corrupt(0, b"XSKL").err(), Some(FormatError::Magic));
        assert_eq!(
            corrupt(4, &2u16.to_le_bytes()).err(),
            Some(FormatError::Version(2))
        );
        assert_eq!(corrupt(6, &1u16.to_le_bytes()).err(), Some(FormatError::Flags(1)));
        assert_eq!(
            corrupt(8, &0u32.to_le_bytes()).err(),
            Some(FormatError::Dimensions)
        );
        assert_eq!(
            corrupt(8, &1025u32.to_le_bytes()).err(),
            Some(FormatError::Dimensions)
        );
        assert_eq!(
            corrupt(12, &1u32.to_le_bytes()).err(),
            Some(FormatError::Reserved)
        );
        // A root that is not first is fine, but bone 0 cannot have a parent, and a parent
        // must precede its child.
        assert_eq!(
            corrupt(16, &0u16.to_le_bytes()).err(),
            Some(FormatError::Inconsistent)
        );
        assert_eq!(
            corrupt(bone1, &1u16.to_le_bytes()).err(),
            Some(FormatError::Inconsistent)
        );
        assert_eq!(
            corrupt(bone1, &2u16.to_le_bytes()).err(),
            Some(FormatError::Inconsistent)
        );
        assert!(
            corrupt(bone1, &NO_PARENT.to_le_bytes()).is_ok(),
            "a second root is allowed"
        );
        assert_eq!(
            corrupt(bone1 + 2, &1u16.to_le_bytes()).err(),
            Some(FormatError::Reserved)
        );
        let hash = bone_name_hash("root");
        assert_eq!(
            corrupt(bone1 + 4, &hash.to_le_bytes()).err(),
            Some(FormatError::DuplicateId(hash))
        );
        assert_eq!(
            corrupt(bone1 + 8, &f32::INFINITY.to_le_bytes()).err(),
            Some(FormatError::NonFinite)
        );
        assert_eq!(
            corrupt(bone1 + 32, &0.9f32.to_le_bytes()).err(),
            Some(FormatError::Geometry),
            "rotation w = 0.9 is not unit"
        );
        assert!(
            corrupt(bone1 + 32, &1.0005f32.to_le_bytes()).is_ok(),
            "within tolerance"
        );
        assert_eq!(
            corrupt(bone1 + 40, &0.0f32.to_le_bytes()).err(),
            Some(FormatError::Geometry)
        );
        assert_eq!(
            corrupt(bone1 + 44, &(-1.0f32).to_le_bytes()).err(),
            Some(FormatError::Geometry)
        );
        assert_eq!(
            corrupt(bone1 + 48 + 12, &0.5f32.to_le_bytes()).err(),
            Some(FormatError::Geometry),
            "inverse bind bottom row element 3"
        );
        assert_eq!(
            corrupt(bone1 + 48 + 60, &2.0f32.to_le_bytes()).err(),
            Some(FormatError::Geometry),
            "inverse bind element 15"
        );
        assert_eq!(
            corrupt(bone1 + 48, &f32::NAN.to_le_bytes()).err(),
            Some(FormatError::NonFinite)
        );
        let mut longer = good.clone();
        longer.push(0);
        assert!(matches!(
            SkeletonAsset::parse(&longer),
            Err(FormatError::Length { .. })
        ));
    }

    #[test]
    fn validate_rejects_in_memory_mistakes() {
        let mut s = chain();
        s.bones.clear();
        assert_eq!(s.validate(), Err(FormatError::Dimensions));
        let mut s = chain();
        if let Some(b) = s.bones.get_mut(2) {
            b.parent = Some(NO_PARENT);
        }
        assert_eq!(s.validate(), Err(FormatError::Inconsistent));
        let mut s = chain();
        if let Some(b) = s.bones.get_mut(1) {
            b.translation = [f32::NAN; 3];
        }
        assert_eq!(s.validate(), Err(FormatError::NonFinite));
    }

    #[test]
    fn no_corruption_or_truncation_panics() {
        let bytes = chain().encode();
        for i in 0..bytes.len() {
            for mask in [0x01u8, 0x80, 0xff] {
                let mut b = bytes.clone();
                if let Some(x) = b.get_mut(i) {
                    *x ^= mask;
                }
                if let Ok(s) = SkeletonAsset::parse(&b) {
                    assert!(SkeletonAsset::parse(&s.encode()).is_ok());
                }
            }
        }
        for len in 0..bytes.len() {
            assert!(
                SkeletonAsset::parse(bytes.get(..len).unwrap_or(&[])).is_err(),
                "truncated to {len}"
            );
        }
    }
}
