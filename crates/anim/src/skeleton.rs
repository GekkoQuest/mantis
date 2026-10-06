//! The runtime skeleton, poses, the model-space pass, and the skinning palette.

use glam::{Affine3A, Mat4};
use mantis_formats::skeleton::{BoneDef, SkeletonAsset};

use crate::error::AnimError;
use crate::transform::Transform;

/// One skinning matrix as uploaded: the top three rows of `model * inverse_bind`
/// (row-major, 48 bytes). The bottom row is always `0, 0, 0, 1`.
pub type PaletteEntry = [[f32; 4]; 3];

/// A validated bone hierarchy with its bind pose.
#[derive(Clone, Debug)]
pub struct Skeleton {
    parents: Vec<Option<usize>>,
    name_hashes: Vec<u32>,
    bind_pose: Vec<Transform>,
    inverse_bind: Vec<Mat4>,
}

impl Skeleton {
    /// Builds a skeleton from a parsed (or in-memory) asset.
    ///
    /// # Errors
    /// [`AnimError::Format`] when the asset breaks a format rule.
    pub fn new(asset: &SkeletonAsset) -> Result<Self, AnimError> {
        asset.validate()?;
        Ok(Self {
            parents: asset.bones.iter().map(|b| b.parent.map(usize::from)).collect(),
            name_hashes: asset.bones.iter().map(|b| b.name_hash).collect(),
            bind_pose: asset.bones.iter().map(bind_local).collect(),
            inverse_bind: asset
                .bones
                .iter()
                .map(|b| Mat4::from_cols_array(&b.inverse_bind))
                .collect(),
        })
    }

    /// Number of bones.
    pub fn bone_count(&self) -> usize {
        self.parents.len()
    }

    /// The parent of `bone`, or `None` for a root or an index out of range.
    pub fn parent(&self, bone: usize) -> Option<usize> {
        self.parents.get(bone).copied().flatten()
    }

    /// True when `ancestor` is `bone` or lies on its parent chain.
    pub fn is_ancestor(&self, ancestor: usize, bone: usize) -> bool {
        let mut at = Some(bone);
        while let Some(b) = at {
            if b == ancestor {
                return true;
            }
            at = self.parent(b);
        }
        false
    }

    /// The name hash of `bone`.
    pub fn name_hash(&self, bone: usize) -> Option<u32> {
        self.name_hashes.get(bone).copied()
    }

    /// The bone with `name_hash` (see [`mantis_formats::skeleton::bone_name_hash`]).
    pub fn find_bone(&self, name_hash: u32) -> Option<usize> {
        self.name_hashes.iter().position(|h| *h == name_hash)
    }

    /// Bind-pose local transforms.
    pub fn bind_pose(&self) -> &[Transform] {
        &self.bind_pose
    }

    /// Inverse bind matrices.
    pub fn inverse_bind(&self) -> &[Mat4] {
        &self.inverse_bind
    }

    /// Model-space transforms from local ones: `model = parent_model * local`, parents
    /// first. Slices shorter than the bone count are processed as far as they reach.
    pub fn model_space(&self, local: &[Transform], model: &mut [Transform]) {
        self.model_space_from(0, local, model);
    }

    /// Recomputes model-space transforms for bones `first..` only (descendants of a bone
    /// always have larger indices, so this refreshes a changed bone's whole subtree).
    pub fn model_space_from(&self, first: usize, local: &[Transform], model: &mut [Transform]) {
        let count = self.bone_count().min(local.len()).min(model.len());
        for bone in first..count {
            let parent_model = self.parent(bone).and_then(|p| model.get(p).copied());
            let (Some(l), Some(m)) = (local.get(bone), model.get_mut(bone)) else {
                return;
            };
            *m = match parent_model {
                Some(pm) => pm * *l,
                None => *l,
            };
        }
    }

    /// Skinning matrices `model * inverse_bind` for upload.
    pub fn skinning_palette(&self, model: &[Transform], palette: &mut [PaletteEntry]) {
        for ((m, inv), out) in model.iter().zip(&self.inverse_bind).zip(palette.iter_mut()) {
            *out = palette_entry(&(m.to_mat4() * *inv));
        }
    }
}

/// The top three rows of an affine matrix.
pub fn palette_entry(m: &Mat4) -> PaletteEntry {
    [m.row(0).to_array(), m.row(1).to_array(), m.row(2).to_array()]
}

fn bind_local(b: &BoneDef) -> Transform {
    Transform::from_arrays(b.translation, b.rotation, b.scale)
}

/// Inverse bind matrices (column-major, bottom row exactly `0, 0, 0, 1`) for bones whose
/// parents and bind transforms are already set, computed with the runtime's TRS
/// convention so the bind pose skins to identity. For cooks and test fixtures.
pub fn inverse_bind_matrices(bones: &[BoneDef]) -> Vec<[f32; 16]> {
    let mut model: Vec<Transform> = Vec::with_capacity(bones.len());
    let mut out = Vec::with_capacity(bones.len());
    for b in bones {
        let local = bind_local(b);
        let m = match b.parent.and_then(|p| model.get(usize::from(p))) {
            Some(pm) => *pm * local,
            None => local,
        };
        model.push(m);
        let affine = Affine3A::from_scale_rotation_translation(m.scale, m.rotation, m.translation);
        out.push(Mat4::from(affine.inverse()).to_cols_array());
    }
    out
}

/// A local-space pose: one transform per bone, allocated once.
#[derive(Clone, PartialEq, Debug)]
pub struct Pose {
    transforms: Vec<Transform>,
}

impl Pose {
    /// A pose of `bone_count` identity transforms.
    pub fn identity(bone_count: usize) -> Self {
        Self {
            transforms: vec![Transform::IDENTITY; bone_count],
        }
    }

    /// The skeleton's bind pose.
    pub fn bind(skeleton: &Skeleton) -> Self {
        Self {
            transforms: skeleton.bind_pose().to_vec(),
        }
    }

    /// Bone count.
    pub fn len(&self) -> usize {
        self.transforms.len()
    }

    /// True for a pose with no bones.
    pub fn is_empty(&self) -> bool {
        self.transforms.is_empty()
    }

    /// The transforms.
    pub fn as_slice(&self) -> &[Transform] {
        &self.transforms
    }

    /// The transforms, mutable.
    pub fn as_mut_slice(&mut self) -> &mut [Transform] {
        &mut self.transforms
    }

    /// Copies `other` into this pose without allocating (up to the shorter length).
    pub fn copy_from(&mut self, other: &[Transform]) {
        for (d, s) in self.transforms.iter_mut().zip(other) {
            *d = *s;
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use glam::{Quat, Vec3};
    use mantis_formats::skeleton::bone_name_hash;

    pub(crate) type TestResult = Result<(), Box<dyn std::error::Error>>;

    /// Builds a skeleton asset from `(parent, name, translation, rotation)` with correct
    /// inverse binds.
    pub(crate) fn asset(bones: &[(Option<u16>, &str, [f32; 3], Quat)]) -> SkeletonAsset {
        let mut defs: Vec<BoneDef> = bones
            .iter()
            .map(|(parent, name, t, r)| BoneDef {
                parent: *parent,
                name_hash: bone_name_hash(name),
                translation: *t,
                rotation: r.to_array(),
                scale: [1.0; 3],
                inverse_bind: [0.0; 16],
            })
            .collect();
        let inv = inverse_bind_matrices(&defs);
        for (d, m) in defs.iter_mut().zip(inv) {
            d.inverse_bind = m;
        }
        SkeletonAsset { bones: defs }
    }

    /// root (origin) -> hip (y 2) -> knee (y -1 below hip) -> ankle (y -1 below knee).
    pub(crate) fn leg() -> Result<Skeleton, AnimError> {
        Skeleton::new(&asset(&[
            (None, "root", [0.0, 0.0, 0.0], Quat::IDENTITY),
            (Some(0), "hip", [0.0, 2.0, 0.0], Quat::IDENTITY),
            (Some(1), "knee", [0.0, -1.0, 0.0], Quat::IDENTITY),
            (Some(2), "ankle", [0.0, -1.0, 0.0], Quat::IDENTITY),
        ]))
    }

    #[test]
    fn model_space_composes_parents_first() -> TestResult {
        let s = leg()?;
        let mut model = vec![Transform::IDENTITY; 4];
        s.model_space(s.bind_pose(), &mut model);
        let ys: Vec<f32> = model.iter().map(|m| m.translation.y).collect();
        assert_eq!(ys, [0.0, 2.0, 1.0, 0.0]);
        let mut local = s.bind_pose().to_vec();
        if let Some(hip) = local.get_mut(1) {
            hip.rotation = Quat::from_rotation_z(core::f32::consts::FRAC_PI_2);
        }
        s.model_space_from(1, &local, &mut model);
        let ankle = model.get(3).ok_or("ankle")?;
        assert!(
            ankle.translation.abs_diff_eq(Vec3::new(2.0, 2.0, 0.0), 1e-5),
            "{ankle:?}"
        );
        Ok(())
    }

    #[test]
    fn bind_pose_palette_is_identity() -> TestResult {
        let s = Skeleton::new(&asset(&[
            (None, "a", [1.0, 0.0, 0.0], Quat::from_rotation_y(0.4)),
            (Some(0), "b", [0.0, 2.0, 0.5], Quat::from_rotation_x(-1.2)),
            (Some(1), "c", [0.3, 0.0, 1.0], Quat::from_rotation_z(2.0)),
        ]))?;
        let mut model = vec![Transform::IDENTITY; 3];
        s.model_space(s.bind_pose(), &mut model);
        let mut palette = vec![[[0.0f32; 4]; 3]; 3];
        s.skinning_palette(&model, &mut palette);
        let identity = palette_entry(&Mat4::IDENTITY);
        for entry in &palette {
            for (row, id) in entry.iter().zip(&identity) {
                for (a, b) in row.iter().zip(id) {
                    assert!((a - b).abs() < 1e-5, "{entry:?}");
                }
            }
        }
        Ok(())
    }

    #[test]
    fn palette_rows_are_the_matrix_rows() {
        let m = Transform::new(Vec3::new(1.0, 2.0, 3.0), Quat::IDENTITY, Vec3::splat(2.0)).to_mat4();
        let e = palette_entry(&m);
        assert_eq!(
            e,
            [[2.0, 0.0, 0.0, 1.0], [0.0, 2.0, 0.0, 2.0], [0.0, 0.0, 2.0, 3.0]]
        );
    }

    #[test]
    fn lookups_and_ancestry() -> TestResult {
        let s = leg()?;
        assert_eq!(s.bone_count(), 4);
        assert_eq!(s.find_bone(bone_name_hash("knee")), Some(2));
        assert_eq!(s.name_hash(3), Some(bone_name_hash("ankle")));
        assert_eq!(s.parent(0), None);
        assert_eq!(s.parent(2), Some(1));
        assert_eq!(s.parent(9), None);
        assert!(s.is_ancestor(1, 3));
        assert!(s.is_ancestor(3, 3));
        assert!(!s.is_ancestor(3, 1));
        Ok(())
    }

    #[test]
    fn rejects_invalid_assets() {
        let mut a = asset(&[(None, "a", [0.0; 3], Quat::IDENTITY)]);
        if let Some(b) = a.bones.get_mut(0) {
            b.rotation = [0.0, 0.0, 0.0, 2.0];
        }
        assert!(matches!(Skeleton::new(&a), Err(AnimError::Format(_))));
    }

    #[test]
    fn pose_helpers() -> TestResult {
        let s = leg()?;
        let mut p = Pose::identity(4);
        assert_eq!(p.len(), 4);
        assert!(!p.is_empty());
        p.copy_from(s.bind_pose());
        assert_eq!(p, Pose::bind(&s));
        assert_eq!(p.as_mut_slice().len(), 4);
        Ok(())
    }
}
