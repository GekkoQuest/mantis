//! Mesh v1: cooked, GPU-ready geometry. Vertices in the renderer's layout, an optional
//! skinning stream, and triangle indices ordered into meshlets, each with a bounding
//! sphere and a normal cone for cluster culling.
//!
//! Spatial convention: positions, normals, and meshlet bounds are model space in the
//! left-handed world frame of decision 0019 (+X right, +Y up, +Z forward). Triangles are
//! wound so that `(b - a) x (c - a)` points outward (clockwise on screen from outside).
//!
//! # Layout (little-endian)
//!
//! | offset | size | field |
//! |---|---|---|
//! | 0 | 4 | magic `"MMSH"` |
//! | 4 | 2 | version `u16` = 1 |
//! | 6 | 2 | flags `u16`: bit 0 skinned (a skinning stream follows the vertices); other bits 0 |
//! | 8 | 4 | vertex count `u32`, 1 to [`MAX_VERTICES`] |
//! | 12 | 4 | index count `u32`, a positive multiple of 3, at most [`MAX_INDICES`] |
//! | 16 | 4 | meshlet count `u32`, 1 to index count / 3 |
//! | 20 | 12 | bounds min `[f32; 3]` |
//! | 32 | 12 | bounds max `[f32; 3]` |
//! | 44 | 4 | reserved, 0 |
//! | 48 | 40 each | vertices: position `[f32; 3]`, normal `[f32; 3]` (unit), uv0 `[f32; 2]`, uv1 `[f32; 2]` |
//! | | 12 each | skinning (when skinned): joints `[u16; 4]`, weights `[u8; 4]` (not all zero) |
//! | | 4 each | indices `u32`, each below the vertex count |
//! | | 40 each | meshlets: first index `u32`, index count `u32` (positive multiple of 3, at most [`MESHLET_MAX_TRIANGLES`] triangles), sphere center `[f32; 3]`, radius `f32`, cone axis `[f32; 3]` (unit, or zero for no cone), cone cutoff `f32` (-1 to 1) |
//!
//! Meshlets cover the index list exactly, in order, without gaps or overlap. Every
//! position lies within the bounds, and every meshlet sphere contains its triangles'
//! vertices (to a relative tolerance of 1e-4). Every `f32` is finite. The length must
//! match exactly.

use crate::bytes::{FormatError, Reader, Writer};

/// Magic bytes.
pub const MAGIC: [u8; 4] = *b"MMSH";
/// Most vertices in one mesh.
pub const MAX_VERTICES: u32 = 1 << 22;
/// Most indices in one mesh.
pub const MAX_INDICES: u32 = 1 << 24;
/// Most triangles in one meshlet.
pub const MESHLET_MAX_TRIANGLES: u32 = 124;
/// Most distinct vertices in one meshlet.
pub const MESHLET_MAX_VERTICES: usize = 64;

/// One vertex.
#[derive(Clone, Copy, PartialEq, Debug, Default)]
pub struct MeshVertex {
    /// Position.
    pub position: [f32; 3],
    /// Unit normal.
    pub normal: [f32; 3],
    /// Texture coordinates.
    pub uv0: [f32; 2],
    /// Lightmap coordinates.
    pub uv1: [f32; 2],
}

/// One vertex's skinning influences.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct SkinInfluence {
    /// Bone indices.
    pub joints: [u16; 4],
    /// Weights, 0 to 255 (renormalized at run time).
    pub weights: [u8; 4],
}

/// A cluster of triangles with culling bounds.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Meshlet {
    /// First index.
    pub first_index: u32,
    /// Indices (three per triangle).
    pub index_count: u32,
    /// Bounding sphere center.
    pub center: [f32; 3],
    /// Bounding sphere radius.
    pub radius: f32,
    /// Normal cone axis (unit), or zero when the cone is open (no backface culling).
    pub cone_axis: [f32; 3],
    /// Cone cutoff: the meshlet is back-facing from any view direction `v` (toward the
    /// meshlet) with `dot(v, axis) >= cutoff`.
    pub cone_cutoff: f32,
}

/// A mesh.
#[derive(Clone, PartialEq, Debug)]
pub struct MeshAsset {
    /// Vertices.
    pub vertices: Vec<MeshVertex>,
    /// Skinning stream, parallel to `vertices`, when skinned.
    pub skin: Option<Vec<SkinInfluence>>,
    /// Triangle indices, meshlet-ordered.
    pub indices: Vec<u32>,
    /// Meshlets covering `indices` in order.
    pub meshlets: Vec<Meshlet>,
    /// Bounds of every position.
    pub bounds_min: [f32; 3],
    /// Bounds of every position.
    pub bounds_max: [f32; 3],
}

fn finite(v: &[f32]) -> bool {
    v.iter().all(|c| c.is_finite())
}

fn dist2(a: [f32; 3], b: [f32; 3]) -> f32 {
    let d = [a[0] - b[0], a[1] - b[1], a[2] - b[2]];
    d[0] * d[0] + d[1] * d[1] + d[2] * d[2]
}

impl MeshAsset {
    /// Checks every rule of the format.
    ///
    /// # Errors
    /// [`FormatError::Dimensions`] for counts, [`FormatError::Geometry`] for indices,
    /// normals, bounds, and meshlets, [`FormatError::NonFinite`], and
    /// [`FormatError::Inconsistent`] for a skinning stream of another length or a vertex
    /// without weight.
    pub fn validate(&self) -> Result<(), FormatError> {
        let vertices = u32::try_from(self.vertices.len()).map_err(|_| FormatError::Dimensions)?;
        let indices = u32::try_from(self.indices.len()).map_err(|_| FormatError::Dimensions)?;
        if vertices == 0
            || vertices > MAX_VERTICES
            || indices == 0
            || indices > MAX_INDICES
            || indices % 3 != 0
        {
            return Err(FormatError::Dimensions);
        }
        if self.meshlets.is_empty() || self.meshlets.len() > self.indices.len() / 3 {
            return Err(FormatError::Dimensions);
        }
        if !finite(&self.bounds_min) || !finite(&self.bounds_max) {
            return Err(FormatError::NonFinite);
        }
        let tol = 1e-4 * (1.0 + dist2(self.bounds_min, self.bounds_max).sqrt());
        for v in &self.vertices {
            if !finite(&v.position) || !finite(&v.normal) || !finite(&v.uv0) || !finite(&v.uv1) {
                return Err(FormatError::NonFinite);
            }
            let len = dist2(v.normal, [0.0; 3]).sqrt();
            let inside = (0..3).all(|i| {
                let p = v.position.get(i).copied().unwrap_or(0.0);
                let lo = self.bounds_min.get(i).copied().unwrap_or(0.0);
                let hi = self.bounds_max.get(i).copied().unwrap_or(0.0);
                p >= lo - tol && p <= hi + tol
            });
            if (len - 1.0).abs() > 1e-3 || !inside {
                return Err(FormatError::Geometry);
            }
        }
        if let Some(skin) = &self.skin
            && (skin.len() != self.vertices.len() || skin.iter().any(|s| s.weights == [0; 4]))
        {
            return Err(FormatError::Inconsistent);
        }
        if self.indices.iter().any(|i| *i >= vertices) {
            return Err(FormatError::Geometry);
        }
        let mut next = 0u32;
        for m in &self.meshlets {
            let ok_count =
                m.index_count > 0 && m.index_count % 3 == 0 && m.index_count / 3 <= MESHLET_MAX_TRIANGLES;
            if m.first_index != next || !ok_count {
                return Err(FormatError::Geometry);
            }
            if !finite(&m.center)
                || !m.radius.is_finite()
                || !finite(&m.cone_axis)
                || !m.cone_cutoff.is_finite()
            {
                return Err(FormatError::NonFinite);
            }
            let axis_len = dist2(m.cone_axis, [0.0; 3]).sqrt();
            if m.radius < 0.0
                || !(axis_len == 0.0 || (axis_len - 1.0).abs() < 1e-3)
                || !(-1.0..=1.0).contains(&m.cone_cutoff)
            {
                return Err(FormatError::Geometry);
            }
            let range = m.first_index as usize..(m.first_index + m.index_count) as usize;
            let r2 = (m.radius * (1.0 + 1e-4) + 1e-5).powi(2);
            for i in self.indices.get(range).ok_or(FormatError::Geometry)? {
                let p = self
                    .vertices
                    .get(*i as usize)
                    .map(|v| v.position)
                    .ok_or(FormatError::Geometry)?;
                if dist2(p, m.center) > r2 {
                    return Err(FormatError::Geometry);
                }
            }
            next += m.index_count;
        }
        if next != indices {
            return Err(FormatError::Geometry);
        }
        Ok(())
    }

    /// Parses and validates.
    ///
    /// # Errors
    /// [`FormatError`] describing the first problem found.
    pub fn parse(bytes: &[u8]) -> Result<MeshAsset, FormatError> {
        let mut r = Reader::new(bytes);
        if r.array::<4>()? != MAGIC {
            return Err(FormatError::Magic);
        }
        let version = r.u16()?;
        if version != 1 {
            return Err(FormatError::Version(version));
        }
        let flags = r.u16()?;
        if flags & !1 != 0 {
            return Err(FormatError::Flags(u32::from(flags)));
        }
        let (vc, ic, mc) = (r.u32()?, r.u32()?, r.u32()?);
        if vc == 0 || vc > MAX_VERTICES || ic == 0 || ic > MAX_INDICES || mc == 0 || mc > ic / 3 {
            return Err(FormatError::Dimensions);
        }
        let bounds_min = r.vec3()?;
        let bounds_max = r.vec3()?;
        if r.u32()? != 0 {
            return Err(FormatError::Reserved);
        }
        let need = u64::from(vc) * 40
            + if flags & 1 != 0 { u64::from(vc) * 12 } else { 0 }
            + u64::from(ic) * 4
            + u64::from(mc) * 40;
        if r.remaining() as u64 != need {
            return Err(FormatError::Length {
                expected: r.position() as u64 + need,
                actual: bytes.len() as u64,
            });
        }
        let mut vertices = Vec::with_capacity(vc as usize);
        for _ in 0..vc {
            vertices.push(MeshVertex {
                position: [r.f32_raw()?, r.f32_raw()?, r.f32_raw()?],
                normal: [r.f32_raw()?, r.f32_raw()?, r.f32_raw()?],
                uv0: [r.f32_raw()?, r.f32_raw()?],
                uv1: [r.f32_raw()?, r.f32_raw()?],
            });
        }
        let skin = if flags & 1 != 0 {
            let mut s = Vec::with_capacity(vc as usize);
            for _ in 0..vc {
                s.push(SkinInfluence {
                    joints: [r.u16()?, r.u16()?, r.u16()?, r.u16()?],
                    weights: r.array()?,
                });
            }
            Some(s)
        } else {
            None
        };
        let mut indices = Vec::with_capacity(ic as usize);
        for _ in 0..ic {
            indices.push(r.u32()?);
        }
        let mut meshlets = Vec::with_capacity(mc as usize);
        for _ in 0..mc {
            meshlets.push(Meshlet {
                first_index: r.u32()?,
                index_count: r.u32()?,
                center: [r.f32_raw()?, r.f32_raw()?, r.f32_raw()?],
                radius: r.f32_raw()?,
                cone_axis: [r.f32_raw()?, r.f32_raw()?, r.f32_raw()?],
                cone_cutoff: r.f32_raw()?,
            });
        }
        r.finish()?;
        let mesh = MeshAsset {
            vertices,
            skin,
            indices,
            meshlets,
            bounds_min,
            bounds_max,
        };
        mesh.validate()?;
        Ok(mesh)
    }

    /// Reference encoder.
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer::new();
        w.bytes(&MAGIC);
        w.u16(1);
        w.u16(u16::from(self.skin.is_some()));
        w.count(self.vertices.len());
        w.count(self.indices.len());
        w.count(self.meshlets.len());
        w.vec3(self.bounds_min);
        w.vec3(self.bounds_max);
        w.u32(0);
        for v in &self.vertices {
            w.vec3(v.position);
            w.vec3(v.normal);
            for c in v.uv0.iter().chain(&v.uv1) {
                w.f32(*c);
            }
        }
        if let Some(skin) = &self.skin {
            for s in skin {
                for j in s.joints {
                    w.u16(j);
                }
                w.bytes(&s.weights);
            }
        }
        for i in &self.indices {
            w.u32(*i);
        }
        for m in &self.meshlets {
            w.u32(m.first_index);
            w.u32(m.index_count);
            w.vec3(m.center);
            w.f32(m.radius);
            w.vec3(m.cone_axis);
            w.f32(m.cone_cutoff);
        }
        w.into_bytes()
    }
}

#[cfg(test)]
mod tests;
