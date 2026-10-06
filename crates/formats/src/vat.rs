//! Vertex animation v1 (MVAT): a baked animation of every vertex of a mesh, played back
//! by the renderer's vertex animation deformation (plan 8.3). Spatial convention:
//! positions, normals, and bounds are model space in the left-handed world frame of
//! decision 0019.
//!
//! # Layout (little-endian)
//!
//! | offset | size | field |
//! |---|---|---|
//! | 0 | 4 | magic `"MVAT"` |
//! | 4 | 2 | version `u16` = 1 |
//! | 6 | 2 | flags `u16`: bit 0 looping (the last frame wraps to the first); other bits 0 |
//! | 8 | 4 | vertex count `u32`, at least 1 |
//! | 12 | 4 | frame count `u32`, at least 1; frames times vertices at most [`MAX_TEXELS`] |
//! | 16 | 4 | seconds per frame `f32`, finite, positive |
//! | 20 | 12 | bounds min `[f32; 3]` |
//! | 32 | 12 | bounds max `[f32; 3]` |
//! | 44 | 4 | reserved, 0 |
//! | 48 | 16 each | positions `[f32; 4]`, frame-major (texel `frame * vertex_count + vertex`), `w` = 1 |
//! | | 16 each | normals `[f32; 4]`, same order, unit or zero, `w` = 0 |
//!
//! Every position lies within the bounds (to a relative tolerance of 1e-4). Every `f32`
//! is finite. The length must match exactly. The texel order is the one
//! `mantis_anim::bake_vat` produces, so a frame is one texture row.

use crate::FormatError;
use crate::bytes::{Reader, Writer};

/// Magic bytes.
pub const MAGIC: [u8; 4] = *b"MVAT";
/// Format version.
pub const VERSION: u16 = 1;
/// Most texels (frames times vertices).
pub const MAX_TEXELS: u64 = 1 << 26;
/// Flag: playback wraps.
pub const FLAG_LOOPING: u16 = 1;
const HEADER: usize = 48;

/// A vertex animation payload.
#[derive(Clone, PartialEq, Debug)]
pub struct VatAsset {
    /// Columns.
    pub vertex_count: u32,
    /// Rows.
    pub frame_count: u32,
    /// Seconds between frames.
    pub seconds_per_frame: f32,
    /// Whether playback wraps from the last frame to the first.
    pub looping: bool,
    /// Minimum corner of every position.
    pub bounds_min: [f32; 3],
    /// Maximum corner of every position.
    pub bounds_max: [f32; 3],
    /// Positions, frame-major, `w = 1`.
    pub positions: Vec<[f32; 4]>,
    /// Unit (or zero) normals, frame-major, `w = 0`.
    pub normals: Vec<[f32; 4]>,
}

fn texels(vertex_count: u32, frame_count: u32) -> Option<usize> {
    let n = u64::from(vertex_count) * u64::from(frame_count);
    (vertex_count > 0 && frame_count > 0 && n <= MAX_TEXELS)
        .then(|| usize::try_from(n).ok())
        .flatten()
}

impl VatAsset {
    /// Checks every rule of the format.
    ///
    /// # Errors
    /// [`FormatError::Dimensions`] for counts, [`FormatError::NonFinite`],
    /// [`FormatError::Keyframes`] for the frame spacing, and [`FormatError::Geometry`]
    /// for `w`, normals, and positions outside the bounds.
    pub fn validate(&self) -> Result<(), FormatError> {
        let n = texels(self.vertex_count, self.frame_count).ok_or(FormatError::Dimensions)?;
        if self.positions.len() != n || self.normals.len() != n {
            return Err(FormatError::Dimensions);
        }
        let all = self
            .bounds_min
            .iter()
            .chain(&self.bounds_max)
            .chain([&self.seconds_per_frame])
            .chain(self.positions.iter().flatten())
            .chain(self.normals.iter().flatten());
        if all.copied().any(|v| !v.is_finite()) {
            return Err(FormatError::NonFinite);
        }
        if self.seconds_per_frame <= 0.0 {
            return Err(FormatError::Keyframes);
        }
        let diag: f32 = self
            .bounds_min
            .iter()
            .zip(self.bounds_max)
            .map(|(a, b)| (b - a) * (b - a))
            .sum::<f32>()
            .sqrt();
        let tol = 1e-4 * (1.0 + diag);
        for p in &self.positions {
            let [px, py, pz, pw] = *p;
            let inside = [px, py, pz]
                .iter()
                .zip(self.bounds_min.iter().zip(self.bounds_max))
                .all(|(v, (lo, hi))| *v >= lo - tol && *v <= hi + tol);
            if pw != 1.0 || !inside {
                return Err(FormatError::Geometry);
            }
        }
        for nrm in &self.normals {
            let [nx, ny, nz, nw] = *nrm;
            let len = (nx * nx + ny * ny + nz * nz).sqrt();
            if nw != 0.0 || !(len == 0.0 || (len - 1.0).abs() <= 1e-3) {
                return Err(FormatError::Geometry);
            }
        }
        Ok(())
    }

    /// Encodes the payload.
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer::new();
        w.bytes(&MAGIC);
        w.u16(VERSION);
        w.u16(if self.looping { FLAG_LOOPING } else { 0 });
        w.u32(self.vertex_count);
        w.u32(self.frame_count);
        w.f32(self.seconds_per_frame);
        w.vec3(self.bounds_min);
        w.vec3(self.bounds_max);
        w.u32(0);
        for v in self.positions.iter().chain(&self.normals).flatten() {
            w.f32(*v);
        }
        w.into_bytes()
    }

    /// Parses and validates a payload.
    ///
    /// # Errors
    /// [`FormatError`] describing the first problem found.
    pub fn parse(bytes: &[u8]) -> Result<VatAsset, FormatError> {
        let mut r = Reader::new(bytes);
        if r.array::<4>()? != MAGIC {
            return Err(FormatError::Magic);
        }
        let version = r.u16()?;
        if version != VERSION {
            return Err(FormatError::Version(version));
        }
        let flags = r.u16()?;
        if flags & !FLAG_LOOPING != 0 {
            return Err(FormatError::Flags(u32::from(flags)));
        }
        let vertex_count = r.u32()?;
        let frame_count = r.u32()?;
        let n = texels(vertex_count, frame_count).ok_or(FormatError::Dimensions)?;
        let seconds_per_frame = r.f32()?;
        let bounds_min = r.vec3()?;
        let bounds_max = r.vec3()?;
        if r.u32()? != 0 {
            return Err(FormatError::Reserved);
        }
        let expected = HEADER as u64 + 32 * n as u64;
        if bytes.len() as u64 != expected {
            return Err(FormatError::Length {
                expected,
                actual: bytes.len() as u64,
            });
        }
        let read = |r: &mut Reader<'_>| -> Result<Vec<[f32; 4]>, FormatError> {
            let mut out = Vec::with_capacity(n);
            for _ in 0..n {
                out.push([r.f32_raw()?, r.f32_raw()?, r.f32_raw()?, r.f32_raw()?]);
            }
            Ok(out)
        };
        let positions = read(&mut r)?;
        let normals = read(&mut r)?;
        r.finish()?;
        let vat = VatAsset {
            vertex_count,
            frame_count,
            seconds_per_frame,
            looping: flags & FLAG_LOOPING != 0,
            bounds_min,
            bounds_max,
            positions,
            normals,
        };
        vat.validate()?;
        Ok(vat)
    }
}
