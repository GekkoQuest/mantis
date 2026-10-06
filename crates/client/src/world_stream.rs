//! World streaming into the renderer (plan 8.3, plan 17): cooked sectors loaded around
//! the camera through the streaming pool and handed to the renderer under the per-frame
//! hand-off budget.
//!
//! - [`WorldIndex`] lists every sector of the verified gameplay bundle (coordinates,
//!   streaming hints) joined by coordinate with its visual container from the verified
//!   presentation bundle (decision 0020: placements, lightmaps, and probes are
//!   presentation content), and maps each material to its texture table. The streamed
//!   hash of a sector is its visual's. A gameplay sector without a visual streams its
//!   collision instead, drawn as debug geometry when [`WorldStreamer::set_debug_collision`]
//!   is on (the default in development builds); a visual without a gameplay sector is
//!   refused ([`WorldError::OrphanVisual`]).
//! - [`WorldStreamer::update`] runs once per frame on the render thread: the renderer's
//!   [`SectorStreamer`] decides what to load and unload around the camera, loads become
//!   [`SectorJob`]s on the [`StreamingPool`] (file reads, hash checks, parsing on worker
//!   threads), and finished sectors are handed off within the budget: meshes, textures,
//!   and materials uploaded once and cached by content hash, the sector's lightmap pages
//!   and probe brick loaded into the renderer's world light, and one instance spawned per
//!   placement (lightmapped placements with their page rectangle).
//! - Every frame each resident placement picks its level of detail from the camera
//!   distance against its `lod_ranges` (times the sector's `lod_distance_scale`), with
//!   [`LOD_HYSTERESIS`] so a camera standing near a switch distance does not flicker
//!   between levels; beyond the last range the placement has no instance. A level change
//!   respawns the instance with that level's mesh, so the batch key follows it.
//! - Unloading despawns the sector's instances and releases its pages and brick. Meshes
//!   and materials are reference-counted by the resident sectors that place them, and
//!   textures by the loaded materials that bind them. An asset no sector uses waits in an
//!   eviction queue (oldest first, up to [`WorldStreamer::set_cache_budget`] assets) so a
//!   sector streaming back in soon finds it still loaded; beyond the budget it is
//!   unloaded and its renderer ranges return to the free lists. Materials compiled at
//!   load ([`WorldStreamer::preload_materials`]) are pinned and never evicted.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use glam::{Mat4, Vec3, Vec4};
use mantis_core::content::ContentHash;
use mantis_formats::bundle::{AssetKind, Bundle};
use mantis_formats::lightmap::Lightmap;
use mantis_formats::material::MaterialAsset;
use mantis_formats::mesh::MeshAsset;
use mantis_formats::probe_volume::ProbeVolume;
use mantis_formats::sector::{PLACEMENT_LIGHTMAPPED, Sector};
use mantis_formats::texture::TextureAsset;
use mantis_render::gpu::Capabilities;
use mantis_render::renderer::{Renderer, RendererError};
use mantis_render::scene::{InstanceHandle, MaterialId, MeshId};
use mantis_render::streaming::{
    MAX_LODS, RequestId, SectorEntry, SectorLoader, SectorStreamer, StreamStats, StreamingConfig,
};
use mantis_render::world_light::LightmapPage;

use crate::content_store::{ContentStore, StoreError};
use crate::threads::streaming::{JobId, Priority, StreamJob, StreamingPool};
use crate::time::HostClock;

/// Fraction a camera must move past a level-of-detail switch distance before the level
/// changes back (both ways), so standing at a boundary does not flicker.
pub const LOD_HYSTERESIS: f32 = 0.1;

/// The level of detail for a placement `distance` away, given its far distances per level
/// (`ranges`, increasing) and its `current` level (`None`: no instance). `fresh` picks
/// without hysteresis (a placement just streamed in). `None` means beyond the last range.
pub fn select_lod(current: Option<usize>, fresh: bool, distance: f32, ranges: &[f32]) -> Option<usize> {
    let target = ranges.iter().position(|r| distance <= *r);
    if fresh || target == current {
        return target;
    }
    let range = |k: usize| ranges.get(k).copied().unwrap_or(f32::MAX);
    match (current, target) {
        // Coarser (or out of range): only once clearly past the current level's range.
        (Some(c), t) if t.is_none_or(|t| t > c) => {
            if distance > range(c) * (1.0 + LOD_HYSTERESIS) {
                target
            } else {
                current
            }
        }
        // Finer (or back into range): only once clearly inside the target's range.
        (_, Some(t)) => {
            if distance < range(t) * (1.0 - LOD_HYSTERESIS) {
                target
            } else {
                current
            }
        }
        _ => current,
    }
}

/// Unreferenced meshes and materials kept loaded by default before eviction.
pub const DEFAULT_CACHE_BUDGET: usize = 32;

/// Plan 17: sector stream-in render-thread hand-off per frame.
pub const HANDOFF_BUDGET: Duration = Duration::from_millis(2);

/// Why the world could not be indexed or a sector loaded.
#[derive(Clone, PartialEq, Debug)]
pub enum WorldError {
    /// The store refused a read.
    Store(StoreError),
    /// A payload does not parse.
    Format {
        /// The payload.
        hash: ContentHash,
        /// The parser's error.
        error: String,
    },
    /// The renderer refused an upload.
    Render(String),
    /// A visual container names a sector the gameplay bundle does not have.
    OrphanVisual((i32, i32)),
}

impl core::fmt::Display for WorldError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            WorldError::Store(e) => write!(f, "store: {e}"),
            WorldError::Format { hash, error } => write!(f, "{hash}: {error}"),
            WorldError::Render(e) => write!(f, "renderer: {e}"),
            WorldError::OrphanVisual(at) => write!(f, "visual for sector {at:?} without a gameplay sector"),
        }
    }
}

impl std::error::Error for WorldError {}

impl From<StoreError> for WorldError {
    fn from(e: StoreError) -> Self {
        WorldError::Store(e)
    }
}

fn format_error(hash: ContentHash, e: impl core::fmt::Display) -> WorldError {
    WorldError::Format {
        hash,
        error: e.to_string(),
    }
}

/// The world as the cooked bundles describe it.
#[derive(Clone, Debug)]
pub struct WorldIndex {
    /// Every sector, for the streamer.
    pub sectors: Vec<SectorEntry>,
    /// Sector edge length (meters), shared by every sector.
    pub sector_size: f32,
    /// Every material a visual sector places, in hash order (for
    /// [`WorldStreamer::preload_materials`]). Materials the world never draws (characters,
    /// effects, UI) are not the streamer's to compile.
    pub materials: Vec<ContentHash>,
    /// Streamed hashes that are gameplay sectors without a visual (collision only).
    pub collision_only: BTreeSet<ContentHash>,
}

fn sector_coords(name: &str, suffix: &str) -> Option<(i32, i32)> {
    let stem = name.strip_prefix("sectors/")?.strip_suffix(suffix)?;
    let (x, z) = stem.split_once('_')?;
    Some((x.parse().ok()?, z.parse().ok()?))
}

impl WorldIndex {
    /// Indexes the client sector copies of `gameplay` (reading each sector's header),
    /// joined with the visual containers of `presentation`.
    ///
    /// # Errors
    /// [`WorldError`] for an unreadable or malformed sector, or
    /// [`WorldError::OrphanVisual`].
    #[expect(clippy::cast_precision_loss)] // Sector coordinates are far below 2^24.
    pub fn build(store: &ContentStore, gameplay: &Bundle, presentation: &Bundle) -> Result<Self, WorldError> {
        let mut visuals: BTreeMap<(i32, i32), ContentHash> = presentation
            .entries
            .iter()
            .filter(|e| e.kind == AssetKind::Sector)
            .filter_map(|e| Some((sector_coords(&e.name, ".visual")?, e.hash)))
            .collect();
        let mut sectors = Vec::new();
        let mut collision_only = BTreeSet::new();
        let mut sector_size = 0.0f32;
        let mut materials = BTreeSet::new();
        for entry in &gameplay.entries {
            if entry.kind != AssetKind::Sector || sector_coords(&entry.name, ".sector").is_none() {
                continue;
            }
            let bytes = store.get(&entry.hash)?;
            let sector = Sector::parse(&bytes).map_err(|e| format_error(entry.hash, e))?;
            let info = sector.info;
            sector_size = info.sector_size;
            let hints = sector.streaming;
            let streamed = if let Some(visual) = visuals.remove(&(info.sector_x, info.sector_z)) {
                let bytes = store.get(&visual)?;
                let placed = Sector::parse(&bytes).map_err(|e| format_error(visual, e))?;
                materials.extend(placed.placements.iter().flatten().map(|p| p.material));
                visual
            } else {
                collision_only.insert(entry.hash);
                entry.hash
            };
            let lods = [streamed; MAX_LODS];
            sectors.push(SectorEntry {
                coord: (info.sector_x, info.sector_z),
                lods,
                lod_count: 1,
                center: Vec3::new(
                    (info.sector_x as f32 + 0.5) * info.sector_size,
                    0.0,
                    (info.sector_z as f32 + 0.5) * info.sector_size,
                ),
                priority_bias: hints.map_or(0.0, |h| h.priority_bias),
                lod_distance_scale: hints.map_or(1.0, |h| h.lod_distance_scale),
            });
        }
        if let Some(at) = visuals.keys().next() {
            return Err(WorldError::OrphanVisual(*at));
        }
        Ok(Self {
            sectors,
            sector_size,
            materials: materials.into_iter().collect(),
            collision_only,
        })
    }
}

/// A material and the textures of its slots.
#[derive(Clone, Debug)]
pub struct LoadedMaterial {
    /// The material's hash.
    pub hash: ContentHash,
    /// The asset.
    pub asset: MaterialAsset,
    /// Texture of each slot.
    pub slots: [Option<ContentHash>; 4],
}

/// A sector read and parsed off-thread, with every asset it names that was not yet
/// resident when the job ran.
#[derive(Clone, Debug)]
pub struct LoadedSector {
    /// The sector's content hash.
    pub hash: ContentHash,
    /// The client copy.
    pub sector: Sector,
    /// Meshes.
    pub meshes: Vec<(ContentHash, MeshAsset)>,
    /// Textures.
    pub textures: Vec<(ContentHash, TextureAsset)>,
    /// Materials.
    pub materials: Vec<LoadedMaterial>,
    /// Lightmaps.
    pub lightmaps: Vec<(ContentHash, Lightmap)>,
    /// The probe volume.
    pub probes: Option<ProbeVolume>,
    /// Debug geometry of a collision-only sector: meshes with their world translation,
    /// keyed by the hash of their encoding.
    pub debug: Vec<(ContentHash, MeshAsset, [f32; 3])>,
    /// Payload bytes read (the hand-off cost estimate).
    pub bytes: usize,
}

/// Loads one sector on a streaming worker.
#[derive(Debug)]
pub struct SectorJob {
    store: Arc<ContentStore>,
    hash: ContentHash,
    resident: Arc<Mutex<BTreeSet<ContentHash>>>,
    /// A collision-only sector: `Some(true)` draws debug geometry, `Some(false)` nothing.
    collision: Option<bool>,
}

/// A box mesh spanning `lo` to `hi` (world space), faces outward.
fn box_mesh(lo: [f32; 3], hi: [f32; 3]) -> MeshAsset {
    let [x0, y0, z0] = lo;
    let [x1, y1, z1] = hi;
    // Each face: outward normal and four corners, clockwise seen from outside in the
    // left-handed world (decision 0019), so (b - a) x (c - a) points outward.
    let faces: [([f32; 3], [[f32; 3]; 4]); 6] = [
        (
            [1.0, 0.0, 0.0],
            [[x1, y0, z0], [x1, y1, z0], [x1, y1, z1], [x1, y0, z1]],
        ),
        (
            [-1.0, 0.0, 0.0],
            [[x0, y0, z1], [x0, y1, z1], [x0, y1, z0], [x0, y0, z0]],
        ),
        (
            [0.0, 1.0, 0.0],
            [[x0, y1, z0], [x0, y1, z1], [x1, y1, z1], [x1, y1, z0]],
        ),
        (
            [0.0, -1.0, 0.0],
            [[x0, y0, z1], [x0, y0, z0], [x1, y0, z0], [x1, y0, z1]],
        ),
        (
            [0.0, 0.0, 1.0],
            [[x1, y0, z1], [x1, y1, z1], [x0, y1, z1], [x0, y0, z1]],
        ),
        (
            [0.0, 0.0, -1.0],
            [[x0, y0, z0], [x0, y1, z0], [x1, y1, z0], [x1, y0, z0]],
        ),
    ];
    let mut vertices = Vec::with_capacity(24);
    let mut indices = Vec::with_capacity(36);
    for (normal, corners) in faces {
        let base = u32::try_from(vertices.len()).unwrap_or(0);
        for (k, position) in corners.into_iter().enumerate() {
            let uv = [f32::from(u8::from(k == 1 || k == 2)), f32::from(u8::from(k >= 2))];
            vertices.push(mantis_formats::mesh::MeshVertex {
                position,
                normal,
                uv0: uv,
                uv1: uv,
            });
        }
        indices.extend_from_slice(&[base, base + 1, base + 2, base, base + 2, base + 3]);
    }
    let center = [
        f32::midpoint(x0, x1),
        f32::midpoint(y0, y1),
        f32::midpoint(z0, z1),
    ];
    let (dx, dy, dz) = (x1 - x0, y1 - y0, z1 - z0);
    let radius = 0.5 * (dx * dx + dy * dy + dz * dz).sqrt();
    MeshAsset {
        vertices,
        skin: None,
        indices,
        meshlets: vec![mantis_formats::mesh::Meshlet {
            first_index: 0,
            index_count: 36,
            center,
            radius,
            cone_axis: [0.0; 3],
            cone_cutoff: -1.0,
        }],
        bounds_min: lo,
        bounds_max: hi,
    }
}

/// Debug geometry of a gameplay sector: its ground and its hull bounds.
fn collision_geometry(sector: &Sector) -> Vec<(ContentHash, MeshAsset, [f32; 3])> {
    let mut out = Vec::new();
    if let Some(g) = &sector.ground {
        let mesh = mantis_formats::sector::ground::mesh(g);
        out.push((
            ContentHash::of(&mesh.encode()),
            mesh,
            [g.origin_x, 0.0, g.origin_z],
        ));
    }
    for h in sector.hulls.iter().flatten() {
        let mesh = box_mesh(h.aabb_min, h.aabb_max);
        out.push((ContentHash::of(&mesh.encode()), mesh, [0.0; 3]));
    }
    out
}

/// The texture of each slot of a material (its version 2 bindings).
fn material_slots(asset: &MaterialAsset) -> [Option<mantis_formats::material::TextureRef>; 4] {
    let mut slots = [None; 4];
    for (slot, t) in slots.iter_mut().zip(&asset.bindings.textures) {
        *slot = Some(*t);
    }
    slots
}

/// Loads a material's texture, checking it is what the material expects (sRGB or
/// normal map; the cook checks this too).
fn load_texture(
    bytes: &[u8],
    expected: mantis_formats::material::TextureRef,
) -> Result<TextureAsset, WorldError> {
    let texture = TextureAsset::parse(bytes).map_err(|e| format_error(expected.hash, e))?;
    if u32::from(texture.flags) != expected.flags {
        return Err(WorldError::Format {
            hash: expected.hash,
            error: format!(
                "texture flags {:#x}, but the material expects {:#x}",
                texture.flags, expected.flags
            ),
        });
    }
    Ok(texture)
}

impl SectorJob {
    fn wanted(&self, hash: ContentHash, seen: &mut BTreeSet<ContentHash>) -> bool {
        let resident = self
            .resident
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .contains(&hash);
        !resident && seen.insert(hash)
    }

    fn load(self) -> Result<LoadedSector, WorldError> {
        let bytes = self.store.get(&self.hash)?;
        let mut total = bytes.len();
        let sector = Sector::parse(&bytes).map_err(|e| format_error(self.hash, e))?;
        let mut seen = BTreeSet::new();
        let mut out = LoadedSector {
            hash: self.hash,
            sector: sector.clone(),
            meshes: Vec::new(),
            textures: Vec::new(),
            materials: Vec::new(),
            lightmaps: Vec::new(),
            probes: None,
            debug: Vec::new(),
            bytes: 0,
        };
        if let Some(draw) = self.collision {
            if draw {
                out.debug = collision_geometry(&sector);
            }
            out.sector = sector.gameplay();
            out.bytes = total;
            return Ok(out);
        }
        let mut read = |hash: &ContentHash| -> Result<Vec<u8>, WorldError> {
            let b = self.store.get(hash)?;
            total += b.len();
            Ok(b)
        };
        for p in sector.placements.iter().flatten() {
            for m in p.meshes() {
                if self.wanted(m, &mut seen) {
                    let mesh = MeshAsset::parse(&read(&m)?).map_err(|e| format_error(m, e))?;
                    out.meshes.push((m, mesh));
                }
            }
            if self.wanted(p.material, &mut seen) {
                let asset =
                    MaterialAsset::parse(&read(&p.material)?).map_err(|e| format_error(p.material, e))?;
                let refs = material_slots(&asset);
                for t in refs.iter().flatten() {
                    if self.wanted(t.hash, &mut seen) {
                        let texture = load_texture(&read(&t.hash)?, *t)?;
                        out.textures.push((t.hash, texture));
                    }
                }
                out.materials.push(LoadedMaterial {
                    hash: p.material,
                    asset,
                    slots: refs.map(|r| r.map(|t| t.hash)),
                });
            }
        }
        for l in sector.lightmaps.iter().flatten() {
            let lightmap = Lightmap::parse(&read(l)?).map_err(|e| format_error(*l, e))?;
            out.lightmaps.push((*l, lightmap));
        }
        if let Some(p) = sector.probe_volume {
            out.probes = Some(ProbeVolume::parse(&read(&p)?).map_err(|e| format_error(p, e))?);
        }
        out.bytes = total;
        Ok(out)
    }
}

impl StreamJob for SectorJob {
    type Output = Result<LoadedSector, WorldError>;

    fn run(self) -> Self::Output {
        self.load()
    }
}

/// The streamer's side of the pool: requests in, unloads recorded for the hand-off.
struct Loader {
    pool: StreamingPool<SectorJob>,
    store: Arc<ContentStore>,
    resident: Arc<Mutex<BTreeSet<ContentHash>>>,
    collision_only: Arc<BTreeSet<ContentHash>>,
    debug_collision: bool,
    requests: BTreeMap<JobId, RequestId>,
    jobs: BTreeMap<RequestId, JobId>,
    next: u64,
    unloads: Vec<ContentHash>,
}

#[expect(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // Clamped to u32.
fn priority(p: f32) -> Priority {
    Priority((p.clamp(0.0, 1.0) * 1_000_000.0) as u32)
}

impl SectorLoader for Loader {
    fn request(&mut self, hash: ContentHash, p: f32) -> RequestId {
        let id = RequestId(self.next);
        self.next = self.next.wrapping_add(1);
        let job = self.pool.submit(
            SectorJob {
                store: Arc::clone(&self.store),
                hash,
                resident: Arc::clone(&self.resident),
                collision: self
                    .collision_only
                    .contains(&hash)
                    .then_some(self.debug_collision),
            },
            priority(p),
        );
        self.requests.insert(job, id);
        self.jobs.insert(id, job);
        id
    }

    fn reprioritize(&mut self, id: RequestId, p: f32) {
        if let Some(job) = self.jobs.get(&id) {
            let _ = self.pool.reprioritize(*job, priority(p));
        }
    }

    fn cancel(&mut self, id: RequestId) {
        if let Some(job) = self.jobs.remove(&id) {
            let _ = self.pool.cancel(job);
            // A job already running still completes; its output is dropped at hand-off.
            let _ = self.requests.remove(&job);
        }
    }

    fn unload(&mut self, hash: ContentHash) {
        self.unloads.push(hash);
    }
}

/// One placement's level-of-detail state.
#[derive(Debug)]
struct Placed {
    /// Mesh of each level (`lod_count` of them).
    meshes: [Option<MeshId>; MAX_LODS],
    /// Far distance of each level, already scaled by the sector's distance scale.
    ranges: [f32; MAX_LODS],
    levels: usize,
    material: MaterialId,
    model: Mat4,
    position: Vec3,
    lightmap: Option<(LightmapPage, [f32; 2], [f32; 2])>,
    level: Option<usize>,
    handle: Option<InstanceHandle>,
}

impl Placed {
    /// Moves to `level` (respawning with that level's mesh, or despawning).
    fn set_level(&mut self, renderer: &mut Renderer, level: Option<usize>) -> Result<(), RendererError> {
        if let Some(h) = self.handle.take() {
            renderer.despawn(h)?;
        }
        self.level = level;
        let Some(mesh) = level.and_then(|l| self.meshes.get(l).copied().flatten()) else {
            return Ok(());
        };
        let handle = match self.lightmap {
            Some((page, scale, offset)) => {
                renderer.spawn_lightmapped(mesh, self.material, self.model, page, scale, offset)?
            }
            None => renderer
                .scene_mut()
                .spawn(mesh, self.material, self.model)
                .map_err(RendererError::Scene)?,
        };
        self.handle = Some(handle);
        Ok(())
    }
}

#[derive(Debug, Default)]
struct ResidentSector {
    coord: (i32, i32),
    /// Debug geometry instances.
    instances: Vec<InstanceHandle>,
    /// Placements, with their current level of detail.
    placed: Vec<Placed>,
    pages: Vec<LightmapPage>,
    probes: bool,
    /// Distinct meshes and materials the sector's instances use (one reference each).
    assets: Vec<ContentHash>,
}

/// What the streamer holds resident.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Residency {
    /// Sectors in the renderer.
    pub sectors: u32,
    /// Meshes uploaded.
    pub meshes: u32,
    /// Materials loaded.
    pub materials: u32,
    /// Textures uploaded.
    pub textures: u32,
    /// Meshes and materials no resident sector uses, awaiting eviction.
    pub unreferenced: u32,
    /// Assets evicted since the streamer started.
    pub evicted: u64,
}

/// One frame's streaming outcome.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct WorldStats {
    /// The streamer's decisions.
    pub stream: StreamStats,
    /// Sectors handed off this frame.
    pub handed_off: u32,
    /// Sectors whose load or hand-off failed this frame.
    pub failed: u32,
    /// Sectors unloaded this frame.
    pub unloaded: u32,
    /// Render-thread hand-off time this frame.
    pub handoff: Duration,
    /// Finished sectors left for a later frame.
    pub backlog: bool,
    /// Sectors resident in the renderer.
    pub resident: u32,
    /// Instances spawned for resident sectors.
    pub instances: u32,
    /// Assets evicted this frame.
    pub evicted: u32,
    /// Placements whose level of detail changed this frame (including appearing or
    /// disappearing at the far range).
    pub lod_changes: u32,
}

/// The GPU objects a hand-off needs.
pub struct Gpu<'a> {
    /// The device.
    pub device: &'a wgpu::Device,
    /// The queue.
    pub queue: &'a wgpu::Queue,
    /// Its optional capabilities.
    pub capabilities: Capabilities,
}

/// Assets to upload: meshes, textures, materials.
type Uploads<'a> = (
    &'a [(ContentHash, MeshAsset)],
    &'a [(ContentHash, TextureAsset)],
    &'a [LoadedMaterial],
);

/// What the renderer holds for the world: uploaded assets by content hash and the
/// resident sectors.
struct Cache {
    meshes: BTreeMap<ContentHash, MeshId>,
    materials: BTreeMap<ContentHash, MaterialId>,
    textures: BTreeMap<ContentHash, (wgpu::Texture, wgpu::TextureView)>,
    sectors: BTreeMap<ContentHash, ResidentSector>,
    errors: Vec<WorldError>,
    /// The flat material collision debug geometry is drawn with.
    debug_material: Option<MaterialId>,
    /// Shared with the workers, so they skip assets already uploaded.
    resident: Arc<Mutex<BTreeSet<ContentHash>>>,
    /// Resident sectors using each mesh and material.
    refs: BTreeMap<ContentHash, u32>,
    /// Loaded materials using each texture.
    texture_refs: BTreeMap<ContentHash, u32>,
    /// The textures each loaded material binds.
    material_textures: BTreeMap<ContentHash, [Option<ContentHash>; 4]>,
    /// Materials never evicted (compiled at load).
    pinned: BTreeSet<ContentHash>,
    /// Unreferenced meshes and materials, oldest first.
    unused: VecDeque<ContentHash>,
    /// Most unreferenced assets kept before evicting.
    budget: usize,
    /// Each sector's level-of-detail distance scale (`STRM`).
    lod_scales: BTreeMap<(i32, i32), f32>,
    /// The camera of the current update (levels of detail are chosen from it).
    camera: Vec3,
    /// Level changes this update.
    lod_changes: u32,
    evicted: u64,
}

/// Streams the world around the camera into the renderer.
pub struct WorldStreamer {
    streamer: SectorStreamer,
    config: StreamingConfig,
    entries: Vec<SectorEntry>,
    loader: Loader,
    cache: Cache,
    sector_size: f32,
    materials: Vec<ContentHash>,
}

impl core::fmt::Debug for WorldStreamer {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("WorldStreamer")
            .field("resident", &self.cache.sectors.len())
            .field("meshes", &self.cache.meshes.len())
            .field("materials", &self.cache.materials.len())
            .finish_non_exhaustive()
    }
}

/// A placement's model matrix (`[x axis, y axis, z axis, translation]`).
fn placement_matrix(t: &[f32; 12]) -> Mat4 {
    let col = |i: usize, w: f32| {
        Vec4::new(
            t.get(i).copied().unwrap_or(0.0),
            t.get(i + 1).copied().unwrap_or(0.0),
            t.get(i + 2).copied().unwrap_or(0.0),
            w,
        )
    };
    Mat4::from_cols(col(0, 0.0), col(3, 0.0), col(6, 0.0), col(9, 1.0))
}

impl WorldStreamer {
    /// A streamer over `index` reading `store` with `workers` streaming threads.
    ///
    /// # Errors
    /// The OS error if a worker cannot start.
    pub fn new(
        store: ContentStore,
        index: WorldIndex,
        config: StreamingConfig,
        workers: usize,
    ) -> std::io::Result<Self> {
        let resident = Arc::new(Mutex::new(BTreeSet::new()));
        let lod_scales = index
            .sectors
            .iter()
            .map(|e| (e.coord, e.lod_distance_scale))
            .collect();
        Ok(Self {
            streamer: SectorStreamer::new(config, index.sectors.clone()),
            config,
            entries: index.sectors,
            loader: Loader {
                pool: StreamingPool::new(workers)?,
                store: Arc::new(store),
                resident: Arc::clone(&resident),
                collision_only: Arc::new(index.collision_only),
                debug_collision: cfg!(debug_assertions),
                requests: BTreeMap::new(),
                jobs: BTreeMap::new(),
                next: 0,
                unloads: Vec::new(),
            },
            cache: Cache {
                meshes: BTreeMap::new(),
                materials: BTreeMap::new(),
                textures: BTreeMap::new(),
                sectors: BTreeMap::new(),
                errors: Vec::new(),
                debug_material: None,
                resident,
                refs: BTreeMap::new(),
                texture_refs: BTreeMap::new(),
                material_textures: BTreeMap::new(),
                pinned: BTreeSet::new(),
                unused: VecDeque::new(),
                budget: DEFAULT_CACHE_BUDGET,
                lod_scales,
                camera: Vec3::ZERO,
                lod_changes: 0,
                evicted: 0,
            },
            sector_size: index.sector_size,
            materials: index.materials,
        })
    }

    /// Whether collision-only sectors (no visual) draw their ground and hull bounds as
    /// debug geometry; on by default in development builds. Applies to loads requested
    /// from now on.
    pub fn set_debug_collision(&mut self, on: bool) {
        self.loader.debug_collision = on;
    }

    /// Forgets everything resident, for a renderer that was rebuilt (a resize): every
    /// cache and residency record refers to the old renderer. Jobs in flight finish and
    /// are dropped; the world streams in again from the next update.
    pub fn restart(&mut self) {
        self.streamer = SectorStreamer::new(self.config, self.entries.clone());
        self.loader.requests.clear();
        self.loader.jobs.clear();
        self.loader.unloads.clear();
        self.cache
            .resident
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clear();
        self.cache.meshes.clear();
        self.cache.materials.clear();
        self.cache.textures.clear();
        self.cache.sectors.clear();
        self.cache.debug_material = None;
        self.cache.refs.clear();
        self.cache.texture_refs.clear();
        self.cache.material_textures.clear();
        self.cache.pinned.clear();
        self.cache.unused.clear();
    }

    /// Unloads everything this streamer put into `renderer`: every resident sector, then
    /// every mesh, material (pinned ones too), and texture, so the renderer's ranges and
    /// slots return to its free lists. Afterwards the streamer is as after
    /// [`WorldStreamer::restart`]. Used when the world is replaced in a live renderer (an
    /// editor's recook). Returns the assets unloaded; renderer refusals are recorded as
    /// errors ([`WorldStreamer::take_errors`]).
    pub fn release_all(&mut self, renderer: &mut Renderer, gpu: &Gpu<'_>) -> u32 {
        let sectors: Vec<ContentHash> = self.cache.sectors.keys().copied().collect();
        for hash in sectors {
            let _ = self.cache.unload(renderer, hash);
        }
        let before = self.cache.evicted;
        self.cache.pinned.clear();
        let assets: Vec<ContentHash> = self
            .cache
            .meshes
            .keys()
            .chain(self.cache.materials.keys())
            .copied()
            .collect();
        for hash in assets {
            self.cache.evict(renderer, gpu, hash);
        }
        if let Some(id) = self.cache.debug_material.take()
            && let Err(e) = renderer.remove_material(gpu.device, id)
        {
            self.cache.errors.push(WorldError::Render(e.to_string()));
        }
        let released = u32::try_from(self.cache.evicted - before).unwrap_or(u32::MAX);
        self.restart();
        released
    }

    /// Keeps up to `assets` unreferenced meshes and materials loaded before evicting the
    /// oldest (0 evicts as soon as no sector uses one).
    pub fn set_cache_budget(&mut self, assets: usize) {
        self.cache.budget = assets;
    }

    /// The renderer material loaded for the cooked material `hash`, if loaded (an editor
    /// replaces it in place while its source is edited).
    pub fn material(&self, hash: &ContentHash) -> Option<MaterialId> {
        self.cache.materials.get(hash).copied()
    }

    /// The level of detail of every resident placement (`None`: beyond its last range).
    pub fn placement_levels(&self) -> Vec<Option<usize>> {
        self.cache.levels()
    }

    /// What is resident now.
    pub fn residency(&self) -> Residency {
        let n = |v: usize| u32::try_from(v).unwrap_or(u32::MAX);
        Residency {
            sectors: n(self.cache.sectors.len()),
            meshes: n(self.cache.meshes.len()),
            materials: n(self.cache.materials.len()),
            textures: n(self.cache.textures.len()),
            unreferenced: n(self.cache.unused.len()),
            evicted: self.cache.evicted,
        }
    }

    /// Sector coordinates resident in the renderer, in order.
    pub fn resident(&self) -> Vec<(i32, i32)> {
        let mut out: Vec<_> = self.cache.sectors.values().map(|s| s.coord).collect();
        out.sort_unstable();
        out
    }

    /// Load and hand-off failures since the last call.
    pub fn take_errors(&mut self) -> Vec<WorldError> {
        core::mem::take(&mut self.cache.errors)
    }

    /// Plans loads around `camera` (moving at `velocity`, looking along `view`), unloads
    /// what left the radius, and hands off finished sectors until `budget` (measured on
    /// `clock`) is spent. The hand-off itself (uploads, world light, spawns) runs inside
    /// the budgeted drain, so [`WorldStats::handoff`] is the render-thread cost.
    pub fn update(
        &mut self,
        renderer: &mut Renderer,
        gpu: &Gpu<'_>,
        clock: &dyn HostClock,
        budget: Duration,
        (camera, velocity, view): (Vec3, Vec3, Vec3),
    ) -> WorldStats {
        let mut stats = WorldStats {
            stream: self.streamer.update(camera, velocity, view, &mut self.loader),
            ..WorldStats::default()
        };
        let evicted_before = self.cache.evicted;
        self.cache.camera = camera;
        self.cache.update_lods(renderer);
        stats.lod_changes = core::mem::take(&mut self.cache.lod_changes);
        for hash in core::mem::take(&mut self.loader.unloads) {
            if self.cache.unload(renderer, hash) {
                stats.unloaded += 1;
            }
        }
        self.cache.trim(renderer, gpu);
        let mut failed: Vec<JobId> = Vec::new();
        let (cache, streamer) = (&mut self.cache, &mut self.streamer);
        let (requests, jobs) = (&mut self.loader.requests, &mut self.loader.jobs);
        let (mut handed, mut refused) = (0u32, 0u32);
        let report = self.loader.pool.drain_budgeted(
            clock,
            budget,
            |out| match out {
                // A rough cost model: uploads scale with the bytes read.
                Ok(s) => Duration::from_nanos(u64::try_from(s.bytes).unwrap_or(u64::MAX).saturating_mul(2)),
                Err(_) => Duration::ZERO,
            },
            |job, out| {
                let Some(request) = requests.remove(&job) else {
                    return; // cancelled while running
                };
                let _ = jobs.remove(&request);
                let ok = match out.and_then(|s| cache.hand_off(renderer, gpu, s)) {
                    Ok(()) => true,
                    Err(e) => {
                        cache.errors.push(e);
                        false
                    }
                };
                handed += u32::from(ok);
                refused += u32::from(!ok);
                let _ = streamer.complete(request, ok);
            },
            |job| failed.push(job),
        );
        stats.handed_off = handed;
        stats.failed = refused;
        for request in failed
            .into_iter()
            .filter_map(|job| self.loader.requests.remove(&job))
        {
            let _ = self.loader.jobs.remove(&request);
            stats.failed += 1;
            let _ = self.streamer.complete(request, false);
        }
        self.cache.trim(renderer, gpu);
        stats.evicted = u32::try_from(self.cache.evicted - evicted_before).unwrap_or(u32::MAX);
        stats.handoff = report.elapsed;
        stats.backlog = report.remaining;
        stats.resident = u32::try_from(self.cache.sectors.len()).unwrap_or(u32::MAX);
        stats.instances = u32::try_from(
            self.cache
                .sectors
                .values()
                .map(|s| s.instances.len() + s.placed.iter().filter(|p| p.handle.is_some()).count())
                .sum::<usize>(),
        )
        .unwrap_or(u32::MAX);
        stats
    }

    /// Sector edge length of the indexed world.
    pub fn sector_size(&self) -> f32 {
        self.sector_size
    }

    /// Loads and compiles every material of the world (with its textures) now, on the
    /// calling thread: pipeline compilation is far over a frame's hand-off budget, so it
    /// belongs to loading, not to streaming. Returns the materials compiled.
    ///
    /// # Errors
    /// [`WorldError`] for an unreadable or malformed material or texture, or a renderer
    /// refusal; nothing after the failure is loaded.
    pub fn preload_materials(&mut self, renderer: &mut Renderer, gpu: &Gpu<'_>) -> Result<usize, WorldError> {
        let store = Arc::clone(&self.loader.store);
        let mut textures: Vec<(ContentHash, TextureAsset)> = Vec::new();
        let mut materials = Vec::new();
        for hash in &self.materials {
            if self.cache.materials.contains_key(hash) {
                continue;
            }
            let asset = MaterialAsset::parse(&store.get(hash)?).map_err(|e| format_error(*hash, e))?;
            let refs = material_slots(&asset);
            for t in refs.iter().flatten() {
                if !self.cache.textures.contains_key(&t.hash) && !textures.iter().any(|(h, _)| *h == t.hash) {
                    textures.push((t.hash, load_texture(&store.get(&t.hash)?, *t)?));
                }
            }
            materials.push(LoadedMaterial {
                hash: *hash,
                asset,
                slots: refs.map(|r| r.map(|t| t.hash)),
            });
        }
        let count = materials.len();
        self.cache
            .upload_assets(renderer, gpu, (&[], &textures, &materials))?;
        for m in &materials {
            self.cache.pinned.insert(m.hash);
        }
        Ok(count)
    }
}

impl Cache {
    fn mark_resident(&self, hash: ContentHash) {
        self.resident
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(hash);
    }

    fn upload_assets(
        &mut self,
        renderer: &mut Renderer,
        gpu: &Gpu<'_>,
        (meshes, textures, materials): Uploads<'_>,
    ) -> Result<(), WorldError> {
        let render = |e: &dyn core::fmt::Display| WorldError::Render(e.to_string());
        for (hash, mesh) in meshes {
            if !self.meshes.contains_key(hash) {
                let id = renderer.add_mesh_asset(gpu.queue, mesh).map_err(|e| render(&e))?;
                self.meshes.insert(*hash, id);
                self.mark_resident(*hash);
            }
        }
        for (hash, texture) in textures {
            if !self.textures.contains_key(hash) {
                let t =
                    mantis_render::assets::upload_texture(gpu.device, gpu.queue, gpu.capabilities, texture)
                        .map_err(|e| render(&e))?;
                self.textures.insert(*hash, t);
                self.mark_resident(*hash);
            }
        }
        for m in materials {
            if self.materials.contains_key(&m.hash) {
                continue;
            }
            let views = m
                .slots
                .map(|slot| slot.and_then(|h| self.textures.get(&h)).map(|(_, v)| v));
            let id = renderer
                .add_material(gpu.device, gpu.queue, m.asset.clone(), views)
                .map_err(|e| render(&e))?;
            self.materials.insert(m.hash, id);
            self.mark_resident(m.hash);
            let mut distinct = m.slots;
            for (i, t) in m.slots.iter().enumerate() {
                if m.slots.iter().take(i).any(|earlier| earlier == t)
                    && let Some(d) = distinct.get_mut(i)
                {
                    *d = None;
                }
            }
            for t in distinct.iter().flatten() {
                *self.texture_refs.entry(*t).or_insert(0) += 1;
            }
            self.material_textures.insert(m.hash, distinct);
        }
        Ok(())
    }

    fn hand_off(
        &mut self,
        renderer: &mut Renderer,
        gpu: &Gpu<'_>,
        mut s: LoadedSector,
    ) -> Result<(), WorldError> {
        if self.sectors.contains_key(&s.hash) {
            return Ok(());
        }
        self.upload_assets(renderer, gpu, (&s.meshes, &s.textures, &s.materials))?;
        let info = s.sector.info;
        let mut resident = ResidentSector {
            coord: (info.sector_x, info.sector_z),
            ..ResidentSector::default()
        };
        let probes = s.probes.take();
        let result = self
            .spawn_sector(renderer, gpu, &s, probes, &mut resident)
            .and_then(|()| self.spawn_debug(renderer, gpu, &s, &mut resident));
        match result {
            Ok(()) => {
                let mut assets: Vec<ContentHash> = s
                    .sector
                    .placements
                    .iter()
                    .flatten()
                    .flat_map(|p| p.meshes().chain([p.material]))
                    .chain(s.debug.iter().map(|d| d.0))
                    .collect();
                assets.sort_unstable();
                assets.dedup();
                for a in &assets {
                    *self.refs.entry(*a).or_insert(0) += 1;
                    self.unused.retain(|u| u != a);
                }
                resident.assets = assets;
                self.sectors.insert(s.hash, resident);
                Ok(())
            }
            Err(e) => {
                // Fail closed: nothing of a half-spawned sector stays in the renderer, and
                // assets uploaded for it that nothing uses queue for eviction.
                release(renderer, &mut resident);
                let uploaded = s
                    .meshes
                    .iter()
                    .map(|m| m.0)
                    .chain(s.materials.iter().map(|m| m.hash));
                for a in uploaded {
                    if !self.refs.contains_key(&a) && !self.pinned.contains(&a) && !self.unused.contains(&a) {
                        self.unused.push_back(a);
                    }
                }
                Err(e)
            }
        }
    }

    fn spawn_sector(
        &self,
        renderer: &mut Renderer,
        gpu: &Gpu<'_>,
        s: &LoadedSector,
        probes: Option<ProbeVolume>,
        resident: &mut ResidentSector,
    ) -> Result<(), WorldError> {
        let render = |e: &dyn core::fmt::Display| WorldError::Render(e.to_string());
        let mut pages = BTreeMap::new();
        for (hash, lightmap) in &s.lightmaps {
            let page = renderer
                .world_light_mut()
                .add_lightmap(gpu.queue, lightmap)
                .map_err(|e| render(&e))?;
            resident.pages.push(page);
            pages.insert(*hash, page);
        }
        if let Some(volume) = probes {
            renderer
                .world_light_mut()
                .add_probes(gpu.queue, resident.coord, s.sector.info.sector_size, volume)
                .map_err(|e| render(&e))?;
            resident.probes = true;
        }
        let scale = self.lod_scales.get(&resident.coord).copied().unwrap_or(1.0);
        for p in s.sector.placements.iter().flatten() {
            let missing = || WorldError::Render(format!("placement names a missing asset in {}", s.hash));
            let material = *self.materials.get(&p.material).ok_or_else(missing)?;
            let mut meshes = [None; MAX_LODS];
            let mut ranges = [f32::MAX; MAX_LODS];
            for (level, (slot, range)) in meshes.iter_mut().zip(ranges.iter_mut()).enumerate() {
                if let Some(hash) = p.mesh_at(level) {
                    *slot = Some(*self.meshes.get(&hash).ok_or_else(missing)?);
                    *range = p.lod_ranges.get(level).copied().unwrap_or(f32::MAX) * scale;
                }
            }
            let model = placement_matrix(&p.transform);
            let lightmap = (p.flags & PLACEMENT_LIGHTMAPPED != 0)
                .then(|| pages.get(&p.lightmap).copied())
                .flatten()
                .map(|page| (page, p.uv_scale, p.uv_offset));
            let levels = usize::try_from(p.lod_count).unwrap_or(1).min(MAX_LODS);
            let mut placed = Placed {
                meshes,
                ranges,
                levels,
                material,
                model,
                position: model.w_axis.truncate(),
                lightmap,
                level: None,
                handle: None,
            };
            let level = select_lod(
                None,
                true,
                placed.position.distance(self.camera),
                placed.ranges.get(..levels).unwrap_or(&[]),
            );
            let result = placed.set_level(renderer, level);
            // Kept even on failure, so release finds whatever was spawned.
            resident.placed.push(placed);
            result.map_err(|e| render(&e))?;
        }
        Ok(())
    }

    /// Uploads and spawns a collision-only sector's debug geometry.
    fn spawn_debug(
        &mut self,
        renderer: &mut Renderer,
        gpu: &Gpu<'_>,
        s: &LoadedSector,
        resident: &mut ResidentSector,
    ) -> Result<(), WorldError> {
        let render = |e: &dyn core::fmt::Display| WorldError::Render(e.to_string());
        if s.debug.is_empty() {
            return Ok(());
        }
        let material = if let Some(m) = self.debug_material {
            m
        } else {
            let m = renderer
                .add_material(
                    gpu.device,
                    gpu.queue,
                    debug_material().map_err(|e| render(&e))?,
                    [None; 4],
                )
                .map_err(|e| render(&e))?;
            self.debug_material = Some(m);
            m
        };
        for (hash, mesh, at) in &s.debug {
            let id = if let Some(id) = self.meshes.get(hash) {
                *id
            } else {
                let id = renderer.add_mesh_asset(gpu.queue, mesh).map_err(|e| render(&e))?;
                self.meshes.insert(*hash, id);
                id
            };
            let [x, y, z] = *at;
            let handle = renderer
                .scene_mut()
                .spawn(id, material, Mat4::from_translation(Vec3::new(x, y, z)))
                .map_err(|e| render(&e))?;
            resident.instances.push(handle);
        }
        Ok(())
    }

    fn unload(&mut self, renderer: &mut Renderer, hash: ContentHash) -> bool {
        match self.sectors.remove(&hash) {
            Some(mut s) => {
                release(renderer, &mut s);
                for a in &s.assets {
                    let gone = self.refs.get_mut(a).is_some_and(|n| {
                        *n = n.saturating_sub(1);
                        *n == 0
                    });
                    if gone {
                        let _ = self.refs.remove(a);
                        if !self.pinned.contains(a) {
                            self.unused.push_back(*a);
                        }
                    }
                }
                true
            }
            None => false,
        }
    }

    /// Moves every resident placement to its level of detail for the camera.
    /// Allocation-free; renderer refusals are recorded as errors.
    fn update_lods(&mut self, renderer: &mut Renderer) {
        for sector in self.sectors.values_mut() {
            for p in &mut sector.placed {
                let ranges = p.ranges.get(..p.levels).unwrap_or(&[]);
                let level = select_lod(p.level, false, p.position.distance(self.camera), ranges);
                if level != p.level {
                    self.lod_changes += 1;
                    if let Err(e) = p.set_level(renderer, level) {
                        self.errors.push(WorldError::Render(e.to_string()));
                    }
                }
            }
        }
    }

    /// The level of detail of every resident placement, sector by sector in streaming
    /// order (for tests and tools).
    fn levels(&self) -> Vec<Option<usize>> {
        self.sectors
            .values()
            .flat_map(|s| s.placed.iter().map(|p| p.level))
            .collect()
    }

    /// Evicts the oldest unreferenced assets beyond the budget.
    fn trim(&mut self, renderer: &mut Renderer, gpu: &Gpu<'_>) {
        while self.unused.len() > self.budget {
            let Some(hash) = self.unused.pop_front() else {
                break;
            };
            self.evict(renderer, gpu, hash);
        }
    }

    fn forget(&self, hash: &ContentHash) {
        self.resident
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(hash);
    }

    /// Unloads one unreferenced mesh or material (and textures no material binds any
    /// more). A refusal from the renderer is recorded and the asset stays loaded.
    fn evict(&mut self, renderer: &mut Renderer, gpu: &Gpu<'_>, hash: ContentHash) {
        if let Some(id) = self.meshes.get(&hash).copied() {
            match renderer.remove_mesh(id) {
                Ok(()) => {
                    let _ = self.meshes.remove(&hash);
                    self.forget(&hash);
                    self.evicted += 1;
                }
                Err(e) => self.errors.push(WorldError::Render(e.to_string())),
            }
            return;
        }
        let Some(id) = self.materials.get(&hash).copied() else {
            return;
        };
        if let Err(e) = renderer.remove_material(gpu.device, id) {
            self.errors.push(WorldError::Render(e.to_string()));
            return;
        }
        let _ = self.materials.remove(&hash);
        self.forget(&hash);
        self.evicted += 1;
        let textures = self.material_textures.remove(&hash).unwrap_or([None; 4]);
        for t in textures.iter().flatten() {
            let gone = self.texture_refs.get_mut(t).is_some_and(|n| {
                *n = n.saturating_sub(1);
                *n == 0
            });
            if gone {
                let _ = self.texture_refs.remove(t);
                let _ = self.textures.remove(t);
                self.forget(t);
                self.evicted += 1;
            }
        }
    }
}

fn release(renderer: &mut Renderer, s: &mut ResidentSector) {
    for h in s.instances.drain(..) {
        let _ = renderer.despawn(h);
    }
    for p in s.placed.drain(..) {
        if let Some(h) = p.handle {
            let _ = renderer.despawn(h);
        }
    }
    for page in s.pages.drain(..) {
        let _ = renderer.world_light_mut().remove_lightmap(page);
    }
    if core::mem::take(&mut s.probes) {
        let _ = renderer.world_light_mut().remove_probes(s.coord);
    }
}

/// The flat, unlit-looking material of collision debug geometry (magenta, Lambert).
fn debug_material() -> Result<MaterialAsset, mantis_formats::material::MaterialError> {
    use mantis_formats::material::{
        Deformations, LightingModel, MaterialGraph, Node, NodeId, SurfaceOutputs, ValueType,
    };
    let graph = MaterialGraph {
        nodes: vec![Node::Constant([0.8, 0.2, 0.7, 0.0], ValueType::Vec3)],
        outputs: SurfaceOutputs {
            base_color: NodeId(0),
            alpha: None,
            emissive: None,
            alpha_cutoff: None,
        },
        lighting: LightingModel::Lambert,
        rim: None,
        outline: None,
    };
    Ok(MaterialAsset {
        graph: graph.validate()?,
        casts_shadows: false,
        deformations: Deformations::NONE,
        bindings: mantis_formats::material::MaterialBindings::default(),
    })
}
