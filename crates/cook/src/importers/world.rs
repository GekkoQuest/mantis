//! World sectors (plan 11, decisions 0016 and 0020): `sectors/<x>_<z>.sector.toml` sources
//! cook to three containers per sector:
//!
//! - the server copy (`sectors/<x>_<z>.server.sector`, server domain) and the client copy
//!   (`sectors/<x>_<z>.sector`, gameplay domain; server-only triggers removed, ground and
//!   collision kept because prediction runs the same kinematics on the same ground), both
//!   holding only the gameplay chunks (`SECT`, `GRND`, `CONV`, `TRIG`, `STRM`);
//! - the visual copy (`sectors/<x>_<z>.visual`, presentation domain): `SECT` plus the
//!   placements, lightmap atlases, and probe volume, so re-baking light or editing a
//!   material never changes the handshake hash.
//!
//! The pipeline enforces the split by chunk id for every importer (see
//! `pipeline::check_sector_domains`).
//!
//! Sectors run at phase 30, after everything they name: meshes and materials (by source
//! path), and the bakes of phase 20 that read the same source (probe volumes and
//! lightmaps, see [`super::bake`]).
//!
//! A ground with a `material` is also cooked to a renderable mesh,
//! `sectors/<x>_<z>.ground.mesh` (presentation domain), in sector-local coordinates
//! (uv0 one unit per cell, uv1 0 to 1 across the sector), and the sector gains a
//! placement naming it at the sector origin (first in the placement list). Ground is lit
//! by the probe volume, never lightmapped.
//!
//! **World rule**: heightfield samples on edges shared with the +x and +z neighbors must
//! match bit for bit; the cook reads the neighbor sources and refuses a world where they
//! do not.
//!
//! # Source syntax
//!
//! ```toml
//! [sector]
//! x = 0                 # grid coordinates; the file must be named `<x>_<z>.sector.toml`
//! z = 0
//! size = 64.0           # world extent along x and z (meters)
//!
//! [ground]              # optional outdoor heightfield
//! cell = 4.0            # sample spacing; size / cell must be a whole number
//! material = "materials/ground.material.toml"   # optional: the ground is drawn
//! flat = 0.0            # every sample at this height, or:
//! # heightmap = "terrain/0_0.pgm"   # an 8-bit PGM input of (size / cell + 1) squared samples
//! # height_scale = 0.1             # meters per gray level
//! # height_offset = 0.0
//!
//! [streaming]           # optional
//! priority_bias = 0.0
//! preload_radius = 32.0
//! lod_distance_scale = 1.0
//!
//! [placement.crate_01]  # one table per placed mesh
//! mesh = "meshes/crate.obj"
//! material = "materials/crate.material.toml"
//! position = [8.0, 0.0, 12.0]
//! yaw = 30.0            # degrees: +Z turns toward +X (decision 0019); also pitch, roll
//! scale = 1.0           # uniform only (placements are similarities)
//! lods = [60.0]         # far view distance per level of detail
//! # lod_meshes = ["meshes/crate_far.obj"]   # meshes of levels 1 and up (default: `mesh`)
//! casts_shadows = true
//! lightmapped = false   # true needs this sector's lightmap bake
//!
//! [hull.wall]           # interior collision: an axis-aligned box
//! min = [0.0, 0.0, 0.0]
//! max = [4.0, 3.0, 0.5]
//! walkable = false
//! blocks_movement = true
//! blocks_sight = true
//!
//! [bake]              # optional lighting bakes, read by `super::bake` at phase 20
//!                       # (keys documented there; `[bake.<keyframe>]` tables allowed)
//!
//! [trigger.door]
//! id = 1
//! kind = 2              # package-defined
//! min = [1.0, 0.0, 0.0]
//! max = [2.0, 2.0, 1.0]
//! server_only = false
//! on_enter = true
//! on_exit = false
//! ```

use std::sync::Arc;

use mantis_core::content::ContentHash;
use mantis_formats::bundle::{AssetKind, Domain};
use mantis_formats::sector::{
    ConvexHull, GroundGrid, HULL_BLOCKS_MOVEMENT, HULL_BLOCKS_SIGHT, HULL_WALKABLE_TOP, MAX_LODS,
    PLACEMENT_CASTS_SHADOWS, PLACEMENT_LIGHTMAPPED, Placement, Sector, SectorInfo, StreamingHints,
    TRIGGER_ON_ENTER, TRIGGER_ON_EXIT, TRIGGER_SERVER_ONLY, Trigger,
};

use crate::importer::{CookError, Cooked, ImportContext, Importer, Source};
use crate::source::{Doc, Fields};

/// The importer version.
pub const VERSION: u32 = 1;

/// Package kind of the lightmap layout payload a bake emits alongside its atlas: one line
/// per lightmapped placement, `<placement name> <scale u> <scale v> <offset u> <offset v>`.
pub const LIGHTMAP_LAYOUT_KIND: AssetKind = AssetKind::Package(1200);

/// This module's importers.
pub fn importers() -> Vec<Arc<dyn Importer>> {
    vec![Arc::new(Sectors)]
}

/// The sector importer.
#[derive(Clone, Copy, Debug, Default)]
pub struct Sectors;

/// Whether `path` is a sector source.
pub fn is_sector_source(path: &str) -> bool {
    path.starts_with("sectors/") && path.ends_with(".sector.toml")
}

/// The grid coordinates a sector source's file name gives.
pub fn coords_of(path: &str) -> Option<(i32, i32)> {
    let name = path.rsplit('/').next()?.strip_suffix(".sector.toml")?;
    let (x, z) = name.split_once('_')?;
    Some((x.parse().ok()?, z.parse().ok()?))
}

/// A sector source's placement, before hashes are resolved.
#[derive(Clone, Debug, PartialEq)]
pub struct PlacementSource {
    /// Item name.
    pub name: String,
    /// Mesh source path (level of detail 0).
    pub mesh: String,
    /// Mesh source paths of levels 1 and up (empty: every level draws `mesh`).
    pub lod_meshes: Vec<String>,
    /// Material source path.
    pub material: String,
    /// World transform: x axis, y axis, z axis, translation.
    pub transform: [f32; 12],
    /// Level-of-detail far distances.
    pub lods: Vec<f32>,
    /// Draws into shadow maps.
    pub casts_shadows: bool,
    /// Uses the sector's lightmap.
    pub lightmapped: bool,
    /// Source line (for errors).
    pub line: usize,
}

/// Everything a sector source declares (shared with the bakes, which read the same
/// sources at phase 20).
#[derive(Clone, Debug, PartialEq)]
pub struct SectorSource {
    /// `SECT` values (content version 0; the bundle carries the package version).
    pub info: SectorInfo,
    /// Ground.
    pub ground: Option<GroundGrid>,
    /// Hulls.
    pub hulls: Vec<ConvexHull>,
    /// Triggers.
    pub triggers: Vec<Trigger>,
    /// Placements.
    pub placements: Vec<PlacementSource>,
    /// Streaming hints.
    pub streaming: Option<StreamingHints>,
    /// The ground's material source path and line, when the ground is drawn.
    pub ground_material: Option<(String, usize)>,
}

fn rotation(yaw: f32, pitch: f32, roll: f32) -> [[f32; 3]; 3] {
    // R = Ry(yaw) * Rx(pitch) * Rz(roll), left-handed: yaw turns +Z toward +X, pitch
    // turns +Z toward -Y (looking down), roll turns +X toward +Y.
    let (sy, cy) = yaw.to_radians().sin_cos();
    let (sp, cp) = pitch.to_radians().sin_cos();
    let (sr, cr) = roll.to_radians().sin_cos();
    let ry = [[cy, 0.0, -sy], [0.0, 1.0, 0.0], [sy, 0.0, cy]]; // columns
    let rx = [[1.0, 0.0, 0.0], [0.0, cp, -sp], [0.0, sp, cp]];
    let rz = [[cr, sr, 0.0], [-sr, cr, 0.0], [0.0, 0.0, 1.0]];
    let mul = |a: [[f32; 3]; 3], b: [[f32; 3]; 3]| {
        let mut out = [[0.0f32; 3]; 3];
        for (col, bc) in out.iter_mut().zip(b) {
            for (row, v) in col.iter_mut().enumerate() {
                *v = (0..3)
                    .map(|k| {
                        a.get(k).and_then(|c| c.get(row)).copied().unwrap_or(0.0)
                            * bc.get(k).copied().unwrap_or(0.0)
                    })
                    .sum();
            }
        }
        out
    };
    mul(mul(ry, rx), rz)
}

fn box_planes(min: [f32; 3], max: [f32; 3]) -> Vec<[f32; 4]> {
    let [x0, y0, z0] = min;
    let [x1, y1, z1] = max;
    vec![
        [1.0, 0.0, 0.0, x1],
        [-1.0, 0.0, 0.0, -x0],
        [0.0, 1.0, 0.0, y1],
        [0.0, -1.0, 0.0, -y0],
        [0.0, 0.0, 1.0, z1],
        [0.0, 0.0, -1.0, -z0],
    ]
}

fn parse_pgm(bytes: &[u8]) -> Option<(u32, u32, &[u8])> {
    // Header: "P5", width, height, maxval 255, each separated by whitespace (comments
    // allowed), then one whitespace byte and the samples.
    let mut fields = Vec::new();
    let mut i = 0usize;
    while fields.len() < 4 {
        while bytes.get(i).is_some_and(u8::is_ascii_whitespace) {
            i += 1;
        }
        if bytes.get(i) == Some(&b'#') {
            while bytes.get(i).is_some_and(|b| *b != b'\n') {
                i += 1;
            }
            continue;
        }
        let start = i;
        while bytes.get(i).is_some_and(|b| !b.is_ascii_whitespace()) {
            i += 1;
        }
        fields.push(core::str::from_utf8(bytes.get(start..i)?).ok()?);
    }
    let [magic, w, h, max] = <[&str; 4]>::try_from(fields.as_slice()).ok()?;
    let (w, h): (u32, u32) = (w.parse().ok()?, h.parse().ok()?);
    if magic != "P5" || max != "255" {
        return None;
    }
    let data = bytes.get(i + 1..)?;
    (data.len() as u64 == u64::from(w) * u64::from(h)).then_some((w, h, data))
}

fn ground(
    f: &Fields<'_>,
    info: &SectorInfo,
    ctx: Option<&ImportContext<'_>>,
    path: &str,
) -> Result<GroundGrid, CookError> {
    f.only(&[
        "cell",
        "flat",
        "heightmap",
        "height_scale",
        "height_offset",
        "material",
    ])?;
    let cell = f.f32("cell")?.0;
    let steps = info.sector_size / cell;
    if cell <= 0.0 || (steps - steps.round()).abs() > 1e-4 || steps < 1.0 || steps > 4096.0 {
        return Err(f.error("cell", "`size / cell` must be a whole number from 1 to 4096"));
    }
    #[expect(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // Checked: whole, 1 to 4096.
    let n = steps.round() as u32 + 1;
    let heights = match (f.has("flat"), f.opt_str("heightmap")?.map(|(s, _)| s)) {
        (true, None) => vec![f.f32("flat")?.0; (n * n) as usize],
        (false, Some(map)) => {
            let ctx = ctx.ok_or_else(|| f.error("heightmap", "heightmaps need the cook context"))?;
            let src = ctx
                .source(map)
                .ok_or_else(|| f.error("heightmap", &format!("`{map}` is not in the content tree")))?;
            let (w, h, data) = parse_pgm(src.bytes).ok_or_else(|| {
                f.error(
                    "heightmap",
                    &format!("`{map}` is not an 8-bit binary PGM (P5, maxval 255)"),
                )
            })?;
            if (w, h) != (n, n) {
                return Err(f.error(
                    "heightmap",
                    &format!("`{map}` is {w}x{h}; this sector needs {n}x{n} samples"),
                ));
            }
            let (scale, offset) = (
                f.f32_or("height_scale", 1.0)?.0,
                f.f32_or("height_offset", 0.0)?.0,
            );
            data.iter().map(|g| f32::from(*g) * scale + offset).collect()
        }
        _ => return Err(f.error("cell", "give exactly one of `flat` or `heightmap`")),
    };
    let _ = path;
    #[expect(clippy::cast_precision_loss)] // Grid coordinates are small.
    Ok(GroundGrid {
        origin_x: info.sector_x as f32 * info.sector_size,
        origin_z: info.sector_z as f32 * info.sector_size,
        cell_size: cell,
        width: n,
        depth: n,
        heights,
    })
}

fn placement(name: &str, f: &Fields<'_>) -> Result<PlacementSource, CookError> {
    f.only(&[
        "mesh",
        "material",
        "position",
        "yaw",
        "pitch",
        "roll",
        "scale",
        "lods",
        "lod_meshes",
        "casts_shadows",
        "lightmapped",
    ])?;
    let position = f.array::<3>("position")?.0;
    let scale = f.f32_or("scale", 1.0)?.0;
    if scale <= 0.0 {
        return Err(f.error("scale", "`scale` must be positive"));
    }
    let r = rotation(
        f.f32_or("yaw", 0.0)?.0,
        f.f32_or("pitch", 0.0)?.0,
        f.f32_or("roll", 0.0)?.0,
    );
    let mut transform = [0.0f32; 12];
    for (slot, v) in transform
        .iter_mut()
        .zip(r.iter().flatten().map(|v| v * scale).chain(position))
    {
        *slot = v;
    }
    let lods = if f.has("lods") {
        f.f32s("lods")?.0
    } else {
        vec![200.0]
    };
    if lods.is_empty()
        || lods.len() > MAX_LODS
        || lods.windows(2).any(|w| matches!(w, [a, b] if a >= b))
        || lods.iter().any(|d| *d <= 0.0)
    {
        return Err(f.error(
            "lods",
            &format!("`lods` holds 1 to {MAX_LODS} positive, increasing distances"),
        ));
    }
    let lod_meshes: Vec<String> = f
        .strs_or_empty("lod_meshes")?
        .0
        .into_iter()
        .map(str::to_owned)
        .collect();
    if !lod_meshes.is_empty() && lod_meshes.len() + 1 != lods.len() {
        return Err(f.error(
            "lod_meshes",
            &format!(
                "`lod_meshes` names the meshes of levels 1 to {}: {} paths, not {}",
                lods.len() - 1,
                lods.len() - 1,
                lod_meshes.len()
            ),
        ));
    }
    Ok(PlacementSource {
        name: name.to_owned(),
        mesh: f.str("mesh")?.0.to_owned(),
        lod_meshes,
        material: f.str("material")?.0.to_owned(),
        transform,
        lods,
        casts_shadows: f.bool_or("casts_shadows", true)?.0,
        lightmapped: f.bool_or("lightmapped", false)?.0,
        line: f.line_of("mesh"),
    })
}

fn aabb(f: &Fields<'_>) -> Result<([f32; 3], [f32; 3]), CookError> {
    let (min, max) = (f.array::<3>("min")?.0, f.array::<3>("max")?.0);
    if min.iter().zip(&max).any(|(a, b)| a > b) {
        return Err(f.error("max", "`max` must not be below `min`"));
    }
    Ok((min, max))
}

fn hull(f: &Fields<'_>) -> Result<ConvexHull, CookError> {
    f.only(&["min", "max", "walkable", "blocks_movement", "blocks_sight"])?;
    let (min, max) = aabb(f)?;
    let mut flags = 0;
    for (key, bit, default) in [
        ("walkable", HULL_WALKABLE_TOP, false),
        ("blocks_movement", HULL_BLOCKS_MOVEMENT, true),
        ("blocks_sight", HULL_BLOCKS_SIGHT, true),
    ] {
        if f.bool_or(key, default)?.0 {
            flags |= bit;
        }
    }
    Ok(ConvexHull {
        aabb_min: min,
        aabb_max: max,
        flags,
        planes: box_planes(min, max),
    })
}

fn trigger(f: &Fields<'_>) -> Result<Trigger, CookError> {
    f.only(&["id", "kind", "min", "max", "server_only", "on_enter", "on_exit"])?;
    let (min, max) = aabb(f)?;
    let mut flags = 0;
    for (key, bit) in [
        ("server_only", TRIGGER_SERVER_ONLY),
        ("on_enter", TRIGGER_ON_ENTER),
        ("on_exit", TRIGGER_ON_EXIT),
    ] {
        if f.bool_or(key, false)?.0 {
            flags |= bit;
        }
    }
    Ok(Trigger {
        id: u32::try_from(f.int_in("id", 0, i64::from(u32::MAX))?.0).unwrap_or(0),
        kind: u16::try_from(f.int_in("kind", 0, i64::from(u16::MAX))?.0).unwrap_or(0),
        flags,
        aabb_min: min,
        aabb_max: max,
    })
}

/// Bake outputs of one sector (phase 20).
struct Bakes {
    probe: Option<ContentHash>,
    atlas: Option<ContentHash>,
    layout: Vec<(String, [f32; 4])>,
}

fn resolve_placement(
    p: &PlacementSource,
    bakes: &Bakes,
    ctx: &ImportContext<'_>,
    path: &str,
) -> Result<Placement, CookError> {
    let mesh = ctx.resolve(&p.mesh, AssetKind::Mesh, path, p.line)?;
    let mut lod_meshes = [ContentHash::ZERO; MAX_LODS - 1];
    for (level, slot) in lod_meshes
        .iter_mut()
        .enumerate()
        .take(p.lods.len().saturating_sub(1))
    {
        *slot = match p.lod_meshes.get(level) {
            Some(source) => ctx.resolve(source, AssetKind::Mesh, path, p.line)?,
            None => mesh,
        };
    }
    let material = ctx.resolve(&p.material, AssetKind::Material, path, p.line)?;
    let mut lod_ranges = [0.0f32; MAX_LODS];
    for (slot, d) in lod_ranges.iter_mut().zip(&p.lods) {
        *slot = *d;
    }
    let mut flags = 0;
    if p.casts_shadows {
        flags |= PLACEMENT_CASTS_SHADOWS;
    }
    let (lightmap, uv_scale, uv_offset) = if p.lightmapped {
        let atlas = bakes.atlas.ok_or_else(|| {
            CookError::at(
                path,
                p.line,
                &format!(
                    "placement `{}` is lightmapped but the sector has no lightmap bake",
                    p.name
                ),
            )
        })?;
        let [su, sv, ou, ov] = bakes
            .layout
            .iter()
            .find(|(n, _)| *n == p.name)
            .map(|(_, r)| *r)
            .ok_or_else(|| {
                CookError::at(
                    path,
                    p.line,
                    &format!("the lightmap bake has no rectangle for `{}`", p.name),
                )
            })?;
        flags |= PLACEMENT_LIGHTMAPPED;
        (atlas, [su, sv], [ou, ov])
    } else {
        (ContentHash::ZERO, [0.0; 2], [0.0; 2])
    };
    Ok(Placement {
        mesh,
        lod_meshes,
        material,
        transform: p.transform,
        lod_count: u32::try_from(p.lods.len()).unwrap_or(1),
        lod_ranges,
        flags,
        lightmap,
        uv_scale,
        uv_offset,
    })
}

/// Parses a sector source (without resolving references). `ctx` is needed for heightmap
/// inputs; the bakes pass theirs too.
///
/// # Errors
/// [`CookError`] located in the source.
pub fn parse_sector(source: &Source<'_>, ctx: Option<&ImportContext<'_>>) -> Result<SectorSource, CookError> {
    let path = source.path;
    let doc = Doc::parse(path, source.text()?)?;
    doc.only_tables(
        &["sector", "ground", "streaming", "bake"],
        &["placement", "hull", "trigger", "bake"],
    )?;
    doc.no_root_keys()?;
    let s = doc.require("sector")?;
    s.only(&["x", "z", "size"])?;
    let (x, z) = (
        s.int_in("x", -1_000_000, 1_000_000)?.0,
        s.int_in("z", -1_000_000, 1_000_000)?.0,
    );
    let info = SectorInfo {
        sector_x: i32::try_from(x).unwrap_or(0),
        sector_z: i32::try_from(z).unwrap_or(0),
        sector_size: s.f32("size")?.0,
        content_version: 0,
    };
    if info.sector_size <= 0.0 {
        return Err(s.error("size", "`size` must be positive"));
    }
    if coords_of(path) != Some((info.sector_x, info.sector_z)) {
        return Err(s.error("x", &format!("the file must be named `{x}_{z}.sector.toml`")));
    }
    let ground = doc
        .table("ground")
        .map(|g| ground(&g, &info, ctx, path))
        .transpose()?;
    let ground_material = match doc.table("ground") {
        Some(g) => g
            .opt_str("material")?
            .map(|(s, _)| s)
            .map(|m| (m.to_owned(), g.line_of("material"))),
        None => None,
    };
    let streaming = doc
        .table("streaming")
        .map(|f| {
            f.only(&["priority_bias", "preload_radius", "lod_distance_scale"])?;
            let hints = StreamingHints {
                priority_bias: f.f32_or("priority_bias", 0.0)?.0,
                preload_radius: f.f32_or("preload_radius", 0.0)?.0,
                lod_distance_scale: f.f32_or("lod_distance_scale", 1.0)?.0,
            };
            if hints.preload_radius < 0.0 || hints.lod_distance_scale <= 0.0 {
                return Err(f.error(
                    "preload_radius",
                    "`preload_radius` >= 0 and `lod_distance_scale` > 0",
                ));
            }
            Ok(hints)
        })
        .transpose()?;
    let placements = doc
        .items("placement")
        .map(|(name, f)| placement(name, &f))
        .collect::<Result<Vec<_>, _>>()?;
    let hulls = doc
        .items("hull")
        .map(|(_, f)| hull(&f))
        .collect::<Result<Vec<_>, _>>()?;
    let triggers = doc
        .items("trigger")
        .map(|(_, f)| trigger(&f))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(SectorSource {
        info,
        ground,
        hulls,
        triggers,
        placements,
        streaming,
        ground_material,
    })
}

/// Far view distance of the ground placement (it streams with its sector).
const GROUND_VIEW_DISTANCE: f32 = 100_000.0;

/// The ground placement: the ground mesh at the sector origin.
fn ground_placement(g: &GroundGrid, mesh: ContentHash, material: ContentHash) -> Placement {
    Placement {
        mesh,
        lod_meshes: [ContentHash::ZERO; MAX_LODS - 1],
        material,
        transform: [
            1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0, g.origin_x, 0.0, g.origin_z,
        ],
        lod_count: 1,
        lod_ranges: [GROUND_VIEW_DISTANCE, 0.0, 0.0, 0.0],
        flags: 0,
        lightmap: ContentHash::ZERO,
        uv_scale: [0.0; 2],
        uv_offset: [0.0; 2],
    }
}

/// Checks the shared edge with a neighbor: `ours` samples at `(i, j)` against `theirs`.
fn shared_edge(ours: &GroundGrid, theirs: &GroundGrid, along_x: bool) -> Option<usize> {
    if ours.width != theirs.width || ours.cell_size.to_bits() != theirs.cell_size.to_bits() {
        return Some(0);
    }
    let n = ours.width as usize;
    (0..n).find(|k| {
        // +x neighbor: our last column against their first; +z: our last row, their first.
        let (a, b) = if along_x {
            (k * n + (n - 1), k * n)
        } else {
            ((n - 1) * n + k, *k)
        };
        ours.heights.get(a).map(|v| v.to_bits()) != theirs.heights.get(b).map(|v| v.to_bits())
    })
}

fn lightmap_layout(bytes: &[u8]) -> Vec<(String, [f32; 4])> {
    core::str::from_utf8(bytes)
        .unwrap_or("")
        .lines()
        .filter_map(|l| {
            let mut it = l.split_whitespace();
            let name = it.next()?.to_owned();
            let v: Vec<f32> = it.filter_map(|t| t.parse().ok()).collect();
            Some((name, <[f32; 4]>::try_from(v.as_slice()).ok()?))
        })
        .collect()
}

impl Importer for Sectors {
    fn name(&self) -> &'static str {
        "world.sector"
    }

    fn version(&self) -> u32 {
        VERSION
    }

    fn phase(&self) -> u32 {
        30
    }

    fn accepts(&self, path: &str) -> bool {
        is_sector_source(path)
    }

    fn inputs(&self, path: &str) -> bool {
        path.starts_with("terrain/")
            && std::path::Path::new(path)
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("pgm"))
    }

    fn import(&self, source: &Source<'_>, ctx: &ImportContext<'_>) -> Result<Vec<Cooked>, CookError> {
        let path = source.path;
        let parsed = parse_sector(source, Some(ctx))?;
        let (x, z) = (parsed.info.sector_x, parsed.info.sector_z);
        // World rule: shared edges with the +x and +z neighbors match bit for bit.
        if let Some(g) = &parsed.ground {
            for (dx, dz, along_x) in [(1, 0, true), (0, 1, false)] {
                let neighbor = format!("sectors/{}_{}.sector.toml", x + dx, z + dz);
                let Some(src) = ctx.source(&neighbor) else {
                    continue;
                };
                let theirs = parse_sector(&src, Some(ctx))?;
                if let Some(tg) = &theirs.ground
                    && let Some(k) = shared_edge(g, tg, along_x)
                {
                    return Err(CookError::at(
                        path,
                        0,
                        &format!("ground does not match `{neighbor}` on the shared edge (sample {k})"),
                    ));
                }
            }
        }
        // Bakes of this sector (phase 20), if any.
        let produced = ctx.produced(path);
        let bakes = Bakes {
            probe: produced
                .iter()
                .find(|p| p.kind == AssetKind::ProbeVolume)
                .map(|p| p.hash),
            atlas: produced
                .iter()
                .find(|p| p.kind == AssetKind::Lightmap)
                .map(|p| p.hash),
            layout: match produced.iter().find(|p| p.kind == LIGHTMAP_LAYOUT_KIND) {
                Some(_) => lightmap_layout(ctx.resolve_bytes(path, LIGHTMAP_LAYOUT_KIND, path, 0)?.1),
                None => Vec::new(),
            },
        };
        let mut placements = parsed
            .placements
            .iter()
            .map(|p| resolve_placement(p, &bakes, ctx, path))
            .collect::<Result<Vec<_>, _>>()?;
        let mut outputs = Vec::new();
        if let (Some(g), Some((material, line))) = (&parsed.ground, &parsed.ground_material) {
            let material = ctx.resolve(material, AssetKind::Material, path, *line)?;
            let mesh = mantis_formats::sector::ground::mesh(g);
            mesh.validate()
                .map_err(|e| CookError::at(path, *line, &format!("ground mesh invalid: {e}")))?;
            let bytes = mesh.encode();
            placements.insert(0, ground_placement(g, ContentHash::of(&bytes), material));
            outputs.push(Cooked {
                name: format!("sectors/{x}_{z}.ground.mesh"),
                kind: AssetKind::Mesh,
                domain: Domain::Presentation,
                bytes,
            });
        }
        let lightmaps: Vec<ContentHash> = bakes.atlas.into_iter().collect();
        let server = Sector {
            info: parsed.info,
            ground: parsed.ground,
            hulls: (!parsed.hulls.is_empty()).then_some(parsed.hulls),
            triggers: (!parsed.triggers.is_empty()).then_some(parsed.triggers),
            placements: (!placements.is_empty()).then_some(placements),
            lightmaps: (!lightmaps.is_empty()).then_some(lightmaps),
            probe_volume: bakes.probe,
            streaming: parsed.streaming,
        };
        let server_bytes = server.gameplay().encode();
        let client_bytes = server.for_client().gameplay().encode();
        let visual_bytes = server.visual().encode();
        for bytes in [&server_bytes, &client_bytes, &visual_bytes] {
            Sector::parse(bytes)
                .map_err(|e| CookError::at(path, 0, &format!("cooked sector does not parse: {e}")))?;
        }
        let stem = format!("sectors/{x}_{z}");
        outputs.extend([
            Cooked {
                name: format!("{stem}.server.sector"),
                kind: AssetKind::Sector,
                domain: Domain::Server,
                bytes: server_bytes,
            },
            Cooked {
                name: format!("{stem}.sector"),
                kind: AssetKind::Sector,
                domain: Domain::Gameplay,
                bytes: client_bytes,
            },
            Cooked {
                name: format!("{stem}.visual"),
                kind: AssetKind::Sector,
                domain: Domain::Presentation,
                bytes: visual_bytes,
            },
        ]);
        Ok(outputs)
    }
}
