//! Vertex animation texture (VAT) baking helpers for the cook (plan 8.4: mid and far crowd
//! tiers play baked vertex animation instead of skinning).
//!
//! [`bake_vat`] samples a clip at a fixed frame rate, skins a mesh with linear-blend
//! skinning at each frame, and lays the results out frame-major: row `f` of the texture
//! is frame `f`, column `v` is vertex `v`, so texel `f * vertex_count + v`. Positions
//! carry `w = 1`, normals `w = 0`. Root motion is stripped, so a baked clip plays in
//! place and the instance transform carries the travel.
//!
//! Frame times: a looping clip bakes `n = max(1, round(duration * fps))` frames at
//! `i * duration / n` (the end is the start again, so it is not repeated); a clamped clip
//! bakes `n + 1` frames at `i * duration / n`, both ends included.

use crate::clip::Clip;
use crate::error::AnimError;
use crate::root_motion::strip_root_motion;
use crate::skeleton::{Pose, Skeleton};
use crate::transform::Transform;
use glam::{Mat4, Vec3, Vec4};

/// Most texels (frames times vertices) one bake may produce.
pub const MAX_VAT_TEXELS: u64 = 1 << 26;

/// A skinned mesh: per-vertex position, normal, up to four bone indices, and weights.
/// Unused influences have weight 0. Weights are normalized during baking; every vertex
/// needs a positive total.
#[derive(Clone, Copy, Debug)]
pub struct SkinnedMesh<'a> {
    /// Bind-pose positions in model space.
    pub positions: &'a [[f32; 3]],
    /// Bind-pose normals in model space.
    pub normals: &'a [[f32; 3]],
    /// Bone indices per vertex.
    pub joints: &'a [[u16; 4]],
    /// Bone weights per vertex.
    pub weights: &'a [[f32; 4]],
}

/// A baked vertex animation.
#[derive(Clone, PartialEq, Debug)]
pub struct VatData {
    /// Rows (frames).
    pub frame_count: u32,
    /// Columns (vertices).
    pub vertex_count: u32,
    /// Seconds between frames.
    pub seconds_per_frame: f32,
    /// Whether playback wraps from the last frame to the first.
    pub looping: bool,
    /// Skinned positions, frame-major, `w = 1`.
    pub positions: Vec<[f32; 4]>,
    /// Skinned unit normals, frame-major, `w = 0`.
    pub normals: Vec<[f32; 4]>,
    /// Minimum corner of every baked position.
    pub bounds_min: [f32; 3],
    /// Maximum corner of every baked position.
    pub bounds_max: [f32; 3],
}

impl VatData {
    /// The texel index of `(frame, vertex)`.
    pub fn texel(&self, frame: u32, vertex: u32) -> Option<usize> {
        (frame < self.frame_count && vertex < self.vertex_count)
            .then(|| frame as usize * self.vertex_count as usize + vertex as usize)
    }

    /// The baked position of `vertex` at `frame`.
    pub fn position(&self, frame: u32, vertex: u32) -> Option<[f32; 4]> {
        self.texel(frame, vertex)
            .and_then(|i| self.positions.get(i).copied())
    }

    /// The baked normal of `vertex` at `frame`.
    pub fn normal(&self, frame: u32, vertex: u32) -> Option<[f32; 4]> {
        self.texel(frame, vertex)
            .and_then(|i| self.normals.get(i).copied())
    }
}

/// Checks a mesh against a skeleton and returns its normalized weights.
fn checked_weights(skeleton: &Skeleton, mesh: &SkinnedMesh<'_>) -> Result<Vec<[f32; 4]>, AnimError> {
    let n = mesh.positions.len();
    if mesh.normals.len() != n || mesh.joints.len() != n || mesh.weights.len() != n {
        return Err(AnimError::InvalidMesh(
            n.min(mesh.normals.len())
                .min(mesh.joints.len())
                .min(mesh.weights.len()),
        ));
    }
    let bones = skeleton.bone_count();
    let mut out = Vec::with_capacity(n);
    for (i, (((p, nrm), j), w)) in mesh
        .positions
        .iter()
        .zip(mesh.normals)
        .zip(mesh.joints)
        .zip(mesh.weights)
        .enumerate()
    {
        let finite = p.iter().chain(nrm).chain(w).all(|v| v.is_finite());
        let total: f32 = w.iter().sum();
        let joints_ok = j
            .iter()
            .zip(w)
            .all(|(b, wt)| *wt == 0.0 || usize::from(*b) < bones);
        if !finite || w.iter().any(|x| *x < 0.0) || total <= 0.0 || !joints_ok {
            return Err(AnimError::InvalidMesh(i));
        }
        out.push(w.map(|x| x / total));
    }
    Ok(out)
}

#[expect(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // Finite, clamped to [1, 2^26] first.
fn frame_intervals(duration: f32, fps: f32) -> u32 {
    let n = (duration * fps).round();
    if n.is_finite() {
        n.clamp(1.0, 67_108_864.0) as u32
    } else {
        1
    }
}

/// Bakes `clip` on `mesh` at `frames_per_second` (see the module docs for layout and
/// frame times).
///
/// # Errors
/// [`AnimError::BoneCountMismatch`] when the clip targets another skeleton,
/// [`AnimError::InvalidFrameRate`] for a non-finite or non-positive rate,
/// [`AnimError::InvalidMesh`] for mismatched attribute lengths, non-finite values,
/// negative or all-zero weights, or a weighted bone index out of range, and
/// [`AnimError::VatTooLarge`] beyond [`MAX_VAT_TEXELS`].
pub fn bake_vat(
    skeleton: &Skeleton,
    clip: &Clip,
    mesh: &SkinnedMesh<'_>,
    frames_per_second: f32,
) -> Result<VatData, AnimError> {
    if clip.bone_count() != skeleton.bone_count() {
        return Err(AnimError::BoneCountMismatch {
            expected: skeleton.bone_count(),
            actual: clip.bone_count(),
        });
    }
    if !frames_per_second.is_finite() || frames_per_second <= 0.0 {
        return Err(AnimError::InvalidFrameRate);
    }
    let weights = checked_weights(skeleton, mesh)?;
    let intervals = frame_intervals(clip.duration(), frames_per_second);
    let frame_count = if clip.is_looping() {
        intervals
    } else {
        intervals + 1
    };
    let vertex_count = u32::try_from(mesh.positions.len()).map_err(|_| AnimError::VatTooLarge)?;
    let texels = u64::from(frame_count) * u64::from(vertex_count);
    if texels > MAX_VAT_TEXELS {
        return Err(AnimError::VatTooLarge);
    }
    let spf = f64::from(clip.duration()) / f64::from(intervals);
    let mut pose = Pose::bind(skeleton);
    let mut model = vec![Transform::IDENTITY; skeleton.bone_count()];
    let mut skin = vec![Mat4::IDENTITY; skeleton.bone_count()];
    let capacity = usize::try_from(texels).map_err(|_| AnimError::VatTooLarge)?;
    let mut positions = Vec::with_capacity(capacity);
    let mut normals = Vec::with_capacity(capacity);
    let mut lo = Vec3::splat(f32::INFINITY);
    let mut hi = Vec3::splat(f32::NEG_INFINITY);
    for frame in 0..frame_count {
        let time = (f64::from(frame) * spf).min(f64::from(clip.duration()));
        #[expect(clippy::cast_possible_truncation)] // A clip time, well within f32.
        clip.sample(skeleton.bind_pose(), time as f32, pose.as_mut_slice());
        if clip.has_root_motion()
            && let Some(root) = pose.as_mut_slice().first_mut()
        {
            strip_root_motion(root);
        }
        skeleton.model_space(pose.as_slice(), &mut model);
        for ((m, inv), s) in model.iter().zip(skeleton.inverse_bind()).zip(skin.iter_mut()) {
            *s = m.to_mat4() * *inv;
        }
        for (((p, n), j), w) in mesh
            .positions
            .iter()
            .zip(mesh.normals)
            .zip(mesh.joints)
            .zip(&weights)
        {
            let (pos, nrm) = skin_vertex(&skin, Vec3::from_array(*p), Vec3::from_array(*n), *j, w);
            lo = lo.min(pos);
            hi = hi.max(pos);
            positions.push(pos.extend(1.0).to_array());
            normals.push(nrm.extend(0.0).to_array());
        }
    }
    if positions.is_empty() {
        lo = Vec3::ZERO;
        hi = Vec3::ZERO;
    }
    Ok(VatData {
        frame_count,
        vertex_count,
        #[expect(clippy::cast_possible_truncation)] // A frame spacing, well within f32.
        seconds_per_frame: spf as f32,
        looping: clip.is_looping(),
        positions,
        normals,
        bounds_min: lo.to_array(),
        bounds_max: hi.to_array(),
    })
}

/// Linear-blend skinning of one vertex. Normals use the matrices' linear part and are
/// renormalized (exact for rotations and uniform scale).
fn skin_vertex(
    skin: &[Mat4],
    position: Vec3,
    normal: Vec3,
    joints: [u16; 4],
    weights: &[f32; 4],
) -> (Vec3, Vec3) {
    let mut p = Vec4::ZERO;
    let mut n = Vec3::ZERO;
    for (j, w) in joints.iter().zip(weights) {
        if *w == 0.0 {
            continue;
        }
        if let Some(m) = skin.get(usize::from(*j)) {
            p += *m * position.extend(1.0) * *w;
            n += m.transform_vector3(normal) * *w;
        }
    }
    (p.truncate(), n.normalize_or_zero())
}

#[cfg(test)]
mod tests;
