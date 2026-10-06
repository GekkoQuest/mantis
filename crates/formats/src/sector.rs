//! Sector container v1: one world sector's cooked data, as typed chunks.
//!
//! Spatial convention: every position, direction, and transform in this format is in the
//! left-handed world frame of decision 0019 (+X right, +Y up, +Z forward, as
//! `mantis_core::kinematics` defines it); placement transforms, ground, collision, and trigger bounds are world space.
//!
//! Two consumers read it: the server (ground and collision for `Motion::step`, triggers)
//! and the client (the same ground for prediction, plus placements, baked-lighting
//! references, and streaming hints). Each reads the chunks it needs; both validate all of
//! them, so one parser defines the format.
//!
//! # Container layout (little-endian)
//!
//! | offset | size | field |
//! |---|---|---|
//! | 0 | 4 | magic `"MSEC"` |
//! | 4 | 2 | version `u16` = 1 |
//! | 6 | 2 | flags `u16` = 0 |
//! | 8 | 4 | chunk count `u32`, 1 to [`MAX_CHUNKS`] |
//! | 12 | 4 | reserved, 0 |
//! | 16 | | chunks |
//!
//! Each chunk: `id: [u8; 4]`, `version: u16` = 1, `flags: u16` = 0, `length: u32` (payload
//! bytes), the payload, then zero padding to the next 4-byte boundary (not counted in
//! `length`). No bytes may follow the last chunk. An unknown chunk id rejects the asset;
//! `SECT` is required; every chunk appears at most once.
//!
//! # Chunks
//!
//! - `SECT` (required): `sector_x: i32`, `sector_z: i32`, `sector_size: f32` (> 0),
//!   `content_version: u32`.
//! - `GRND` (outdoor heightfield): `origin_x: f32`, `origin_z: f32`, `cell_size: f32` (> 0),
//!   `width: u32` (>= 2), `depth: u32` (>= 2), `heights: [f32; width * depth]` row-major
//!   by z (`heights[j * width + i]` is at `(origin_x + i * cell, origin_z + j * cell)`).
//!   Length is exactly `20 + 4 * width * depth`. `sector_size` must equal
//!   `(width - 1) * cell_size` and `(depth - 1) * cell_size`. Adjacent sectors share their
//!   edge samples bit for bit (a cook rule; a single sector cannot check it).
//! - `CONV` (interior convex collision): `hull_count: u32`, then per hull `aabb_min: [f32; 3]`,
//!   `aabb_max: [f32; 3]` (min <= max), `flags: u32` (bit 0 walkable top, bit 1 blocks
//!   movement, bit 2 blocks sight; other bits 0), `plane_count: u32` (4 to 64), `planes:
//!   [[f32; 4]; plane_count]`, each `[nx, ny, nz, d]` with the solid where
//!   `n . p <= d` and `|n| = 1` within 1e-4.
//! - `TRIG` (trigger volumes): `trigger_count: u32`, then per trigger `id: u32` (unique),
//!   `kind: u16` (package-defined), `flags: u16` (bit 0 server only, bit 1 fires on enter,
//!   bit 2 fires on exit; other bits 0), `aabb_min: [f32; 3]`, `aabb_max: [f32; 3]`.
//!   Server-only triggers never ship to clients: see [`Sector::for_client`].
//! - `PLAC` (placements): `count: u32`, then per placement (280 bytes): `mesh: [u8; 32]`
//!   (content hash of level of detail 0), `lod_meshes: [[u8; 32]; 3]` (the meshes of
//!   levels 1 to 3: non-zero for every level below `lod_count`, zero above; a level may
//!   repeat the previous level's mesh), `material: [u8; 32]` (content hash of the
//!   material it is drawn with),
//!   `transform: [f32; 12]` (x axis, y axis, z axis, translation; finite,
//!   non-degenerate, and a similarity: orthogonal axes of equal length, see
//!   [`is_similarity`]. Renderers transform normals by the model matrix, which is only
//!   correct for uniform scale; supporting non-uniform scale requires a normal matrix and
//!   a format version bump), `lod_count: u32` (1 to 4), `lod_ranges: [f32; 4]` (the far view
//!   distance of each level, positive and strictly increasing; unused slots 0),
//!   `flags: u32` (bit 0 lightmapped, bit 1 casts shadows; other bits 0), `lightmap:
//!   [u8; 32]` (an atlas listed in `LMAP` when lightmapped, all zero otherwise),
//!   `uv_scale: [f32; 2]`, `uv_offset: [f32; 2]` (all zero when not lightmapped).
//! - `LMAP` (lightmap atlases): `count: u32`, then `[u8; 32]` content hashes, unique.
//! - `PRBV` (probe volume): one `[u8; 32]` content hash.
//! - `STRM` (streaming hints): `priority_bias: f32`, `preload_radius: f32` (>= 0),
//!   `lod_distance_scale: f32` (> 0).
//!
//! Every `f32` must be finite.
//!
//! # Content domains (decision 0020)
//!
//! A sector is cooked as two containers joined by `SECT` coordinates: the gameplay copy
//! ([`Sector::gameplay`], chunks [`chunk::GAMEPLAY`], in the gameplay and server bundles)
//! and the visual copy ([`Sector::visual`], chunks [`chunk::VISUAL`], in the presentation
//! bundle). A byte is gameplay only if the server or the client's prediction reads it, so
//! visual iteration never changes the handshake hash. [`Sector::chunk_ids`] lets a cook
//! enforce the split by chunk id.

use mantis_core::content::ContentHash;

use crate::bytes::{FormatError, Reader, Writer};

/// Magic bytes.
pub const MAGIC: [u8; 4] = *b"MSEC";
/// Most chunks per container.
pub const MAX_CHUNKS: u32 = 64;
/// Most planes per convex hull.
pub const MAX_HULL_PLANES: u32 = 64;
/// Fewest planes per convex hull.
pub const MIN_HULL_PLANES: u32 = 4;
/// Most levels of detail per placement.
pub const MAX_LODS: usize = 4;

/// Chunk ids.
pub mod chunk {
    /// Sector placement (required).
    pub const SECT: [u8; 4] = *b"SECT";
    /// Outdoor heightfield.
    pub const GRND: [u8; 4] = *b"GRND";
    /// Interior convex collision.
    pub const CONV: [u8; 4] = *b"CONV";
    /// Trigger volumes.
    pub const TRIG: [u8; 4] = *b"TRIG";
    /// Placements.
    pub const PLAC: [u8; 4] = *b"PLAC";
    /// Lightmap atlases.
    pub const LMAP: [u8; 4] = *b"LMAP";
    /// Probe volume.
    pub const PRBV: [u8; 4] = *b"PRBV";
    /// Streaming hints.
    pub const STRM: [u8; 4] = *b"STRM";
    /// Every id this version knows, in canonical order.
    pub const ALL: [[u8; 4]; 8] = [SECT, GRND, CONV, TRIG, PLAC, LMAP, PRBV, STRM];
    /// Chunks of the gameplay and server copies (decision 0020): what the server or the
    /// client's prediction reads.
    pub const GAMEPLAY: [[u8; 4]; 5] = [SECT, GRND, CONV, TRIG, STRM];
    /// Chunks of the visual copy (presentation domain): `SECT` to join by coordinate, plus
    /// everything only drawing reads.
    pub const VISUAL: [[u8; 4]; 4] = [SECT, PLAC, LMAP, PRBV];
}

pub mod ground;

/// `SECT`.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct SectorInfo {
    /// Grid x.
    pub sector_x: i32,
    /// Grid z.
    pub sector_z: i32,
    /// World extent along x and z.
    pub sector_size: f32,
    /// Content version.
    pub content_version: u32,
}

/// `GRND`: the fields of core's `Heightfield::new`, one for one.
#[derive(Clone, PartialEq, Debug)]
pub struct GroundGrid {
    /// World x of sample (0, 0).
    pub origin_x: f32,
    /// World z of sample (0, 0).
    pub origin_z: f32,
    /// Spacing between samples.
    pub cell_size: f32,
    /// Samples along x.
    pub width: u32,
    /// Samples along z.
    pub depth: u32,
    /// Heights, row-major by z.
    pub heights: Vec<f32>,
}

/// Convex hull flag: the top surface is walkable.
pub const HULL_WALKABLE_TOP: u32 = 1 << 0;
/// Convex hull flag: blocks movement.
pub const HULL_BLOCKS_MOVEMENT: u32 = 1 << 1;
/// Convex hull flag: blocks sight.
pub const HULL_BLOCKS_SIGHT: u32 = 1 << 2;

/// One `CONV` hull.
#[derive(Clone, PartialEq, Debug)]
pub struct ConvexHull {
    /// Broadphase minimum.
    pub aabb_min: [f32; 3],
    /// Broadphase maximum.
    pub aabb_max: [f32; 3],
    /// `HULL_*` flags.
    pub flags: u32,
    /// Planes `[nx, ny, nz, d]`; the solid is where `n . p <= d` for all.
    pub planes: Vec<[f32; 4]>,
}

/// Trigger flag: server only, never shipped to clients.
pub const TRIGGER_SERVER_ONLY: u16 = 1 << 0;
/// Trigger flag: fires on enter.
pub const TRIGGER_ON_ENTER: u16 = 1 << 1;
/// Trigger flag: fires on exit.
pub const TRIGGER_ON_EXIT: u16 = 1 << 2;

/// One `TRIG` trigger.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Trigger {
    /// Stable id, unique in the sector.
    pub id: u32,
    /// Package-defined kind.
    pub kind: u16,
    /// `TRIGGER_*` flags.
    pub flags: u16,
    /// Minimum corner.
    pub aabb_min: [f32; 3],
    /// Maximum corner.
    pub aabb_max: [f32; 3],
}

/// Placement flag: lightmapped.
pub const PLACEMENT_LIGHTMAPPED: u32 = 1 << 0;
/// Placement flag: casts shadows.
pub const PLACEMENT_CASTS_SHADOWS: u32 = 1 << 1;

/// One `PLAC` placement.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Placement {
    /// Mesh content hash.
    pub mesh: ContentHash,
    /// The meshes of levels 1 to 3 (non-zero below `lod_count`, zero above).
    pub lod_meshes: [ContentHash; MAX_LODS - 1],
    /// Material content hash.
    pub material: ContentHash,
    /// x axis, y axis, z axis, translation.
    pub transform: [f32; 12],
    /// Levels of detail in use (1 to 4).
    pub lod_count: u32,
    /// Far view distance of each level.
    pub lod_ranges: [f32; MAX_LODS],
    /// `PLACEMENT_*` flags.
    pub flags: u32,
    /// Lightmap atlas, or zero when not lightmapped.
    pub lightmap: ContentHash,
    /// Lightmap UV scale into the atlas.
    pub uv_scale: [f32; 2],
    /// Lightmap UV offset into the atlas.
    pub uv_offset: [f32; 2],
}

/// `STRM`.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct StreamingHints {
    /// Added to the computed streaming priority.
    pub priority_bias: f32,
    /// Distance at which loading starts.
    pub preload_radius: f32,
    /// Multiplier on level-of-detail distances.
    pub lod_distance_scale: f32,
}

impl Placement {
    /// The mesh of level of detail `level` (level 0 is [`Placement::mesh`]); `None`
    /// beyond `lod_count`.
    pub fn mesh_at(&self, level: usize) -> Option<ContentHash> {
        if level >= self.lod_count as usize {
            return None;
        }
        match level {
            0 => Some(self.mesh),
            k => self.lod_meshes.get(k - 1).copied(),
        }
    }

    /// Every distinct mesh the placement may draw.
    pub fn meshes(&self) -> impl Iterator<Item = ContentHash> + '_ {
        (0..self.lod_count as usize).filter_map(|l| self.mesh_at(l))
    }
}

/// A parsed sector.
#[derive(Clone, PartialEq, Debug)]
pub struct Sector {
    /// `SECT`.
    pub info: SectorInfo,
    /// `GRND`.
    pub ground: Option<GroundGrid>,
    /// `CONV`.
    pub hulls: Option<Vec<ConvexHull>>,
    /// `TRIG`.
    pub triggers: Option<Vec<Trigger>>,
    /// `PLAC`.
    pub placements: Option<Vec<Placement>>,
    /// `LMAP`.
    pub lightmaps: Option<Vec<ContentHash>>,
    /// `PRBV`.
    pub probe_volume: Option<ContentHash>,
    /// `STRM`.
    pub streaming: Option<StreamingHints>,
}

fn aabb(r: &mut Reader<'_>) -> Result<([f32; 3], [f32; 3]), FormatError> {
    let (lo, hi) = (r.vec3()?, r.vec3()?);
    if lo.iter().zip(&hi).any(|(a, b)| a > b) {
        return Err(FormatError::Geometry);
    }
    Ok((lo, hi))
}

fn hash(r: &mut Reader<'_>) -> Result<ContentHash, FormatError> {
    Ok(ContentHash::from_bytes(r.array::<32>()?))
}

fn parse_info(r: &mut Reader<'_>) -> Result<SectorInfo, FormatError> {
    let (sector_x, sector_z) = (r.i32()?, r.i32()?);
    let sector_size = r.f32()?;
    if sector_size <= 0.0 {
        return Err(FormatError::Geometry);
    }
    Ok(SectorInfo {
        sector_x,
        sector_z,
        sector_size,
        content_version: r.u32()?,
    })
}

fn parse_ground(r: &mut Reader<'_>, payload_len: usize) -> Result<GroundGrid, FormatError> {
    let (origin_x, origin_z, cell_size) = (r.f32()?, r.f32()?, r.f32()?);
    if cell_size <= 0.0 {
        return Err(FormatError::Geometry);
    }
    let (width, depth) = (r.u32()?, r.u32()?);
    let samples = width.checked_mul(depth).ok_or(FormatError::Dimensions)?;
    if width < 2 || depth < 2 {
        return Err(FormatError::Dimensions);
    }
    let expected = 20 + 4 * u64::from(samples);
    if payload_len as u64 != expected {
        return Err(FormatError::Length {
            expected,
            actual: payload_len as u64,
        });
    }
    let mut heights = Vec::with_capacity(samples as usize);
    for _ in 0..samples {
        heights.push(r.f32()?);
    }
    Ok(GroundGrid {
        origin_x,
        origin_z,
        cell_size,
        width,
        depth,
        heights,
    })
}

fn parse_hulls(r: &mut Reader<'_>) -> Result<Vec<ConvexHull>, FormatError> {
    let count = r.u32()?;
    let mut hulls = Vec::with_capacity((count as usize).min(r.remaining() / 40));
    for _ in 0..count {
        let (aabb_min, aabb_max) = aabb(r)?;
        let flags = r.u32()?;
        if flags & !(HULL_WALKABLE_TOP | HULL_BLOCKS_MOVEMENT | HULL_BLOCKS_SIGHT) != 0 {
            return Err(FormatError::Flags(flags));
        }
        let plane_count = r.u32()?;
        if !(MIN_HULL_PLANES..=MAX_HULL_PLANES).contains(&plane_count) {
            return Err(FormatError::Dimensions);
        }
        let mut planes = Vec::with_capacity(plane_count as usize);
        for _ in 0..plane_count {
            let p = [r.f32()?, r.f32()?, r.f32()?, r.f32()?];
            let [nx, ny, nz, _] = p;
            if ((nx * nx + ny * ny + nz * nz).sqrt() - 1.0).abs() > 1e-4 {
                return Err(FormatError::Geometry);
            }
            planes.push(p);
        }
        hulls.push(ConvexHull {
            aabb_min,
            aabb_max,
            flags,
            planes,
        });
    }
    Ok(hulls)
}

fn parse_triggers(r: &mut Reader<'_>) -> Result<Vec<Trigger>, FormatError> {
    let count = r.u32()?;
    let mut triggers: Vec<Trigger> = Vec::with_capacity((count as usize).min(r.remaining() / 32));
    for _ in 0..count {
        let id = r.u32()?;
        let kind = r.u16()?;
        let flags = r.u16()?;
        if flags & !(TRIGGER_SERVER_ONLY | TRIGGER_ON_ENTER | TRIGGER_ON_EXIT) != 0 {
            return Err(FormatError::Flags(u32::from(flags)));
        }
        if triggers.iter().any(|t| t.id == id) {
            return Err(FormatError::DuplicateId(id));
        }
        let (aabb_min, aabb_max) = aabb(r)?;
        triggers.push(Trigger {
            id,
            kind,
            flags,
            aabb_min,
            aabb_max,
        });
    }
    Ok(triggers)
}

/// Whether three basis axes form a similarity (rotation, optional reflection, and one
/// uniform scale): pairwise orthogonal and of equal length, to a relative tolerance of
/// 1e-4. Placements must satisfy this (non-uniform scale and shear would need a normal
/// matrix the runtime formats do not carry).
pub fn is_similarity(axes: [[f32; 3]; 3]) -> bool {
    let dot = |a: [f32; 3], b: [f32; 3]| a[0] * b[0] + a[1] * b[1] + a[2] * b[2];
    let [x, y, z] = axes;
    let (lx, ly, lz) = (dot(x, x), dot(y, y), dot(z, z));
    let longest = lx.max(ly).max(lz);
    if !(longest.is_finite() && longest > 0.0) {
        return false;
    }
    // Squared lengths equal within 2e-4 relative (lengths within 1e-4).
    let tol = 2e-4 * longest;
    (lx - ly).abs() <= tol
        && (ly - lz).abs() <= tol
        && dot(x, y).abs() <= tol
        && dot(y, z).abs() <= tol
        && dot(x, z).abs() <= tol
}

fn parse_placements(r: &mut Reader<'_>) -> Result<Vec<Placement>, FormatError> {
    let count = r.u32()?;
    let mut out = Vec::with_capacity((count as usize).min(r.remaining() / 280));
    for _ in 0..count {
        let mesh = hash(r)?;
        let lod_meshes = [hash(r)?, hash(r)?, hash(r)?];
        let material = hash(r)?;
        let mut transform = [0.0f32; 12];
        for t in &mut transform {
            *t = r.f32()?;
        }
        let [ax, ay, az, bx, by, bz, cx, cy, cz, ..] = transform;
        let det = ax * (by * cz - bz * cy) - ay * (bx * cz - bz * cx) + az * (bx * cy - by * cx);
        if det.abs() < 1e-12 || !is_similarity([[ax, ay, az], [bx, by, bz], [cx, cy, cz]]) {
            return Err(FormatError::Geometry);
        }
        let lod_count = r.u32()?;
        if lod_count == 0 || lod_count as usize > MAX_LODS {
            return Err(FormatError::Dimensions);
        }
        let mut lod_ranges = [0.0f32; MAX_LODS];
        for slot in &mut lod_ranges {
            *slot = r.f32()?;
        }
        validate_lods(&lod_ranges, lod_count as usize)?;
        let levels_ok = lod_meshes
            .iter()
            .enumerate()
            .all(|(i, h)| (i + 1 < lod_count as usize) != (*h == ContentHash::ZERO));
        if mesh == ContentHash::ZERO || !levels_ok {
            return Err(FormatError::Inconsistent);
        }
        let flags = r.u32()?;
        if flags & !(PLACEMENT_LIGHTMAPPED | PLACEMENT_CASTS_SHADOWS) != 0 {
            return Err(FormatError::Flags(flags));
        }
        let lightmap = hash(r)?;
        let uv_scale = [r.f32()?, r.f32()?];
        let uv_offset = [r.f32()?, r.f32()?];
        let lightmapped = flags & PLACEMENT_LIGHTMAPPED != 0;
        let unset = lightmap == ContentHash::ZERO && uv_scale == [0.0; 2] && uv_offset == [0.0; 2];
        if lightmapped == (lightmap == ContentHash::ZERO) || (!lightmapped && !unset) {
            return Err(FormatError::Inconsistent);
        }
        out.push(Placement {
            mesh,
            lod_meshes,
            material,
            transform,
            lod_count,
            lod_ranges,
            flags,
            lightmap,
            uv_scale,
            uv_offset,
        });
    }
    Ok(out)
}

fn validate_lods(ranges: &[f32; MAX_LODS], count: usize) -> Result<(), FormatError> {
    let mut prev = 0.0f32;
    for (i, v) in ranges.iter().enumerate() {
        if i < count {
            if *v <= prev {
                return Err(FormatError::Geometry);
            }
            prev = *v;
        } else if v.to_bits() != 0 {
            return Err(FormatError::Reserved);
        }
    }
    Ok(())
}

fn parse_hash_list(r: &mut Reader<'_>) -> Result<Vec<ContentHash>, FormatError> {
    let count = r.u32()?;
    let mut out: Vec<ContentHash> = Vec::with_capacity((count as usize).min(r.remaining() / 32));
    for _ in 0..count {
        let h = hash(r)?;
        if out.contains(&h) {
            return Err(FormatError::Inconsistent);
        }
        out.push(h);
    }
    Ok(out)
}

fn parse_streaming(r: &mut Reader<'_>) -> Result<StreamingHints, FormatError> {
    let s = StreamingHints {
        priority_bias: r.f32()?,
        preload_radius: r.f32()?,
        lod_distance_scale: r.f32()?,
    };
    if s.preload_radius < 0.0 || s.lod_distance_scale <= 0.0 {
        return Err(FormatError::Geometry);
    }
    Ok(s)
}

fn set_once<T>(slot: &mut Option<T>, id: [u8; 4], value: T) -> Result<(), FormatError> {
    if slot.is_some() {
        return Err(FormatError::DuplicateChunk(id));
    }
    *slot = Some(value);
    Ok(())
}

impl Sector {
    /// Parses and validates a sector container.
    ///
    /// # Errors
    /// [`FormatError`] describing the first problem found.
    pub fn parse(bytes: &[u8]) -> Result<Sector, FormatError> {
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
        if !(1..=MAX_CHUNKS).contains(&count) {
            return Err(FormatError::Dimensions);
        }
        if r.u32()? != 0 {
            return Err(FormatError::Reserved);
        }
        let mut info = None;
        let mut s = Sector {
            info: SectorInfo {
                sector_x: 0,
                sector_z: 0,
                sector_size: 1.0,
                content_version: 0,
            },
            ground: None,
            hulls: None,
            triggers: None,
            placements: None,
            lightmaps: None,
            probe_volume: None,
            streaming: None,
        };
        for _ in 0..count {
            let id = r.array::<4>()?;
            let chunk_version = r.u16()?;
            if chunk_version != 1 {
                return Err(FormatError::Version(chunk_version));
            }
            let chunk_flags = r.u16()?;
            if chunk_flags != 0 {
                return Err(FormatError::Flags(u32::from(chunk_flags)));
            }
            let len = r.u32()? as usize;
            let payload = r.slice(len)?;
            let pad = len.next_multiple_of(4) - len;
            if r.slice(pad)?.iter().any(|b| *b != 0) {
                return Err(FormatError::Reserved);
            }
            let mut p = Reader::new(payload);
            match id {
                chunk::SECT => set_once(&mut info, id, parse_info(&mut p)?)?,
                chunk::GRND => set_once(&mut s.ground, id, parse_ground(&mut p, len)?)?,
                chunk::CONV => set_once(&mut s.hulls, id, parse_hulls(&mut p)?)?,
                chunk::TRIG => set_once(&mut s.triggers, id, parse_triggers(&mut p)?)?,
                chunk::PLAC => set_once(&mut s.placements, id, parse_placements(&mut p)?)?,
                chunk::LMAP => set_once(&mut s.lightmaps, id, parse_hash_list(&mut p)?)?,
                chunk::PRBV => set_once(&mut s.probe_volume, id, hash(&mut p)?)?,
                chunk::STRM => set_once(&mut s.streaming, id, parse_streaming(&mut p)?)?,
                other => return Err(FormatError::UnknownChunk(other)),
            }
            p.finish()?;
        }
        r.finish()?;
        s.info = info.ok_or(FormatError::MissingChunk(chunk::SECT))?;
        s.validate_cross()?;
        Ok(s)
    }

    #[expect(clippy::cast_precision_loss)] // Sample counts are far below 2^24.
    fn validate_cross(&self) -> Result<(), FormatError> {
        if let Some(g) = &self.ground {
            let size = self.info.sector_size;
            if (g.width - 1) as f32 * g.cell_size != size || (g.depth - 1) as f32 * g.cell_size != size {
                return Err(FormatError::Inconsistent);
            }
        }
        if let Some(placements) = &self.placements {
            let atlases = self.lightmaps.as_deref().unwrap_or(&[]);
            if placements
                .iter()
                .any(|p| p.flags & PLACEMENT_LIGHTMAPPED != 0 && !atlases.contains(&p.lightmap))
            {
                return Err(FormatError::Inconsistent);
            }
        }
        Ok(())
    }

    /// The sector as shipped to clients: identical, minus server-only triggers.
    ///
    /// The client copy and the server copy therefore have different bytes and different
    /// content hashes. Anything that references "the sector hash" must say which copy it
    /// means; the handshake's content-bundle hash covers the client bundle only.
    #[must_use]
    pub fn for_client(&self) -> Sector {
        let mut s = self.clone();
        if let Some(t) = &mut s.triggers {
            t.retain(|t| t.flags & TRIGGER_SERVER_ONLY == 0);
        }
        s
    }

    /// The ids of the chunks present, in canonical order.
    pub fn chunk_ids(&self) -> Vec<[u8; 4]> {
        let present = [
            true,
            self.ground.is_some(),
            self.hulls.is_some(),
            self.triggers.is_some(),
            self.placements.is_some(),
            self.lightmaps.is_some(),
            self.probe_volume.is_some(),
            self.streaming.is_some(),
        ];
        chunk::ALL
            .iter()
            .zip(present)
            .filter_map(|(id, p)| p.then_some(*id))
            .collect()
    }

    /// The gameplay copy: [`chunk::GAMEPLAY`] only (apply [`Sector::for_client`] too for
    /// the client's).
    #[must_use]
    pub fn gameplay(&self) -> Sector {
        Sector {
            placements: None,
            lightmaps: None,
            probe_volume: None,
            ..self.clone()
        }
    }

    /// The visual copy: [`chunk::VISUAL`] only.
    #[must_use]
    pub fn visual(&self) -> Sector {
        Sector {
            info: self.info,
            ground: None,
            hulls: None,
            triggers: None,
            placements: self.placements.clone(),
            lightmaps: self.lightmaps.clone(),
            probe_volume: self.probe_volume,
            streaming: None,
        }
    }

    /// Reference encoder: chunks in canonical order, absent chunks omitted.
    pub fn encode(&self) -> Vec<u8> {
        let mut chunks: Vec<([u8; 4], Vec<u8>)> = vec![(chunk::SECT, write_info(&self.info))];
        if let Some(g) = &self.ground {
            chunks.push((chunk::GRND, write_ground(g)));
        }
        if let Some(h) = &self.hulls {
            chunks.push((chunk::CONV, write_hulls(h)));
        }
        if let Some(t) = &self.triggers {
            chunks.push((chunk::TRIG, write_triggers(t)));
        }
        if let Some(p) = &self.placements {
            chunks.push((chunk::PLAC, write_placements(p)));
        }
        if let Some(list) = &self.lightmaps {
            let mut w = Writer::new();
            w.count(list.len());
            for h in list {
                w.bytes(h.as_bytes());
            }
            chunks.push((chunk::LMAP, w.into_bytes()));
        }
        if let Some(h) = &self.probe_volume {
            chunks.push((chunk::PRBV, h.as_bytes().to_vec()));
        }
        if let Some(s) = &self.streaming {
            let mut w = Writer::new();
            w.f32(s.priority_bias);
            w.f32(s.preload_radius);
            w.f32(s.lod_distance_scale);
            chunks.push((chunk::STRM, w.into_bytes()));
        }
        let mut out = Writer::new();
        out.bytes(&MAGIC);
        out.u16(1);
        out.u16(0);
        out.count(chunks.len());
        out.u32(0);
        for (id, bytes) in chunks {
            out.bytes(&id);
            out.u16(1);
            out.u16(0);
            out.count(bytes.len());
            out.bytes(&bytes);
            for _ in 0..(bytes.len().next_multiple_of(4) - bytes.len()) {
                out.u8(0);
            }
        }
        out.into_bytes()
    }
}

fn write_info(info: &SectorInfo) -> Vec<u8> {
    let mut w = Writer::new();
    w.i32(info.sector_x);
    w.i32(info.sector_z);
    w.f32(info.sector_size);
    w.u32(info.content_version);
    w.into_bytes()
}

fn write_ground(g: &GroundGrid) -> Vec<u8> {
    let mut w = Writer::new();
    w.f32(g.origin_x);
    w.f32(g.origin_z);
    w.f32(g.cell_size);
    w.u32(g.width);
    w.u32(g.depth);
    for h in &g.heights {
        w.f32(*h);
    }
    w.into_bytes()
}

fn write_hulls(hulls: &[ConvexHull]) -> Vec<u8> {
    let mut w = Writer::new();
    w.count(hulls.len());
    for h in hulls {
        w.vec3(h.aabb_min);
        w.vec3(h.aabb_max);
        w.u32(h.flags);
        w.count(h.planes.len());
        for p in &h.planes {
            for c in p {
                w.f32(*c);
            }
        }
    }
    w.into_bytes()
}

fn write_triggers(triggers: &[Trigger]) -> Vec<u8> {
    let mut w = Writer::new();
    w.count(triggers.len());
    for t in triggers {
        w.u32(t.id);
        w.u16(t.kind);
        w.u16(t.flags);
        w.vec3(t.aabb_min);
        w.vec3(t.aabb_max);
    }
    w.into_bytes()
}

fn write_placements(placements: &[Placement]) -> Vec<u8> {
    let mut w = Writer::new();
    w.count(placements.len());
    for p in placements {
        w.bytes(p.mesh.as_bytes());
        for h in &p.lod_meshes {
            w.bytes(h.as_bytes());
        }
        w.bytes(p.material.as_bytes());
        for t in p.transform {
            w.f32(t);
        }
        w.u32(p.lod_count);
        for l in p.lod_ranges {
            w.f32(l);
        }
        w.u32(p.flags);
        w.bytes(p.lightmap.as_bytes());
        for v in p.uv_scale.into_iter().chain(p.uv_offset) {
            w.f32(v);
        }
    }
    w.into_bytes()
}

#[cfg(test)]
mod tests;
