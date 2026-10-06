//! World streaming end to end on the no-op GPU backend: a small synthetic world written
//! as a signed content store (the layout the cook publishes), indexed from its verified
//! bundles, streamed around a moving camera through the streaming pool into the
//! renderer, with unloads, a tampered object, and a wrong key failing closed.

#![allow(clippy::cast_precision_loss, clippy::too_many_lines, clippy::indexing_slicing)] // Test geometry.

use std::path::{Path, PathBuf};
use std::time::Duration;

use glam::Vec3;
use mantis_client::content_store::{ContentStore, StoreError};
use mantis_client::time::ManualClock;
use mantis_client::world_stream::{Gpu, Residency, WorldError, WorldIndex, WorldStats, WorldStreamer};
use mantis_core::content::ContentHash;
use mantis_formats::bundle::{AssetKind, Bundle, Domain, Entry, SIGNATURE_LEN};
use mantis_formats::lightmap::{Lightmap, rgb_to_rgb9e5};
use mantis_formats::material::{
    Deformations, LightingModel, MaterialAsset, MaterialGraph, Node, NodeId, SurfaceOutputs,
};
use mantis_formats::mesh::{MeshAsset, MeshVertex, Meshlet};
use mantis_formats::probe_volume::ProbeVolume;
use mantis_formats::sector::{
    ConvexHull, GroundGrid, HULL_BLOCKS_MOVEMENT, PLACEMENT_CASTS_SHADOWS, PLACEMENT_LIGHTMAPPED, Placement,
    Sector, SectorInfo,
};
use mantis_formats::sh::ShL1;
use mantis_formats::texture::{Encoding, TextureAsset, full_chain, mip_size};
use mantis_render::gpu::GpuContext;
use mantis_render::gpu_test::{noop, validation_errors};
use mantis_render::math::Camera;
use mantis_render::post::PostSettings;
use mantis_render::renderer::{FrameInputs, Renderer, RendererConfig};
use mantis_render::streaming::StreamingConfig;
use ring::signature::KeyPair;

type TestResult = Result<(), Box<dyn std::error::Error>>;
type Error = Box<dyn std::error::Error>;

const SIZE: f32 = 16.0;

struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> std::io::Result<Self> {
        let p = std::env::temp_dir().join(format!("mantis-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p)?;
        Ok(Self(p))
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Writes payloads and signed bundles in the store layout.
struct Writer {
    root: PathBuf,
    entries: [Vec<Entry>; 3],
}

impl Writer {
    fn put(
        &mut self,
        domain: Domain,
        name: &str,
        kind: AssetKind,
        bytes: &[u8],
    ) -> Result<ContentHash, Error> {
        let hash = ContentHash::of(bytes);
        let hex = hash.to_string();
        let dir = self.root.join("objects").join(hex.get(..2).ok_or("hex")?);
        std::fs::create_dir_all(&dir)?;
        std::fs::write(dir.join(&hex), bytes)?;
        let slot = match domain {
            Domain::Gameplay => 0,
            Domain::Server => 1,
            Domain::Presentation => 2,
        };
        if let Some(list) = self.entries.get_mut(slot) {
            list.push(Entry {
                name: name.to_owned(),
                kind,
                size: bytes.len() as u64,
                hash,
            });
        }
        Ok(hash)
    }

    /// Signs and writes the three bundles; returns the public key.
    fn finish(self) -> Result<[u8; 32], Error> {
        let rng = ring::rand::SystemRandom::new();
        let pkcs8 = ring::signature::Ed25519KeyPair::generate_pkcs8(&rng).map_err(|_| "key")?;
        let pair = ring::signature::Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).map_err(|_| "key")?;
        let dir = self.root.join("bundles");
        std::fs::create_dir_all(&dir)?;
        for (mut entries, (domain, name)) in self.entries.into_iter().zip([
            (Domain::Gameplay, "gameplay"),
            (Domain::Server, "server"),
            (Domain::Presentation, "presentation"),
        ]) {
            entries.sort_by(|a, b| a.name.cmp(&b.name));
            let bundle = Bundle {
                domain,
                content_version: 1,
                entries,
            };
            let mut signature = [0u8; SIGNATURE_LEN];
            signature.copy_from_slice(pair.sign(&bundle.signed_bytes()).as_ref());
            std::fs::write(dir.join(format!("{name}.bundle")), bundle.encode(&signature))?;
        }
        let mut key = [0u8; 32];
        key.copy_from_slice(pair.public_key().as_ref());
        Ok(key)
    }
}

fn mesh(vertices: Vec<MeshVertex>, indices: Vec<u32>) -> MeshAsset {
    let (mut lo, mut hi) = ([f32::MAX; 3], [f32::MIN; 3]);
    for v in &vertices {
        for k in 0..3 {
            lo[k] = lo[k].min(v.position[k]);
            hi[k] = hi[k].max(v.position[k]);
        }
    }
    let center = [0, 1, 2].map(|k| f32::midpoint(lo[k], hi[k]));
    let radius = vertices
        .iter()
        .map(|v| {
            (0..3)
                .map(|k| (v.position[k] - center[k]) * (v.position[k] - center[k]))
                .sum::<f32>()
                .sqrt()
        })
        .fold(0.0f32, f32::max);
    let count = u32::try_from(indices.len()).unwrap_or(0);
    MeshAsset {
        vertices,
        skin: None,
        indices,
        meshlets: vec![Meshlet {
            first_index: 0,
            index_count: count,
            center,
            radius,
            cone_axis: [0.0; 3],
            cone_cutoff: -1.0,
        }],
        bounds_min: lo,
        bounds_max: hi,
    }
}

fn convert(v: &[mantis_render::gpu_types::Vertex]) -> Vec<MeshVertex> {
    v.iter()
        .map(|v| MeshVertex {
            position: v.position,
            normal: v.normal,
            uv0: v.uv0,
            uv1: v.uv1,
        })
        .collect()
}

fn material(texture: ContentHash, flags: u32) -> Result<MaterialAsset, Error> {
    let graph = MaterialGraph {
        nodes: vec![
            Node::Uv0,
            Node::Texture {
                slot: 0,
                uv: NodeId(0),
            },
            Node::Swizzle(NodeId(1), [0, 1, 2, 0], 3),
        ],
        outputs: SurfaceOutputs {
            base_color: NodeId(2),
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
        casts_shadows: true,
        deformations: Deformations::NONE,
        bindings: mantis_formats::material::MaterialBindings {
            textures: vec![mantis_formats::material::TextureRef { hash: texture, flags }],
            scalars: Vec::new(),
            colors: Vec::new(),
        },
    })
}

fn placement(
    mesh: ContentHash,
    material: ContentHash,
    at: [f32; 3],
    lightmap: Option<ContentHash>,
) -> Placement {
    Placement {
        mesh,
        lod_meshes: [ContentHash::ZERO; 3],
        material,
        transform: [1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0, at[0], at[1], at[2]],
        lod_count: 1,
        lod_ranges: [100.0, 0.0, 0.0, 0.0],
        flags: PLACEMENT_CASTS_SHADOWS
            | if lightmap.is_some() {
                PLACEMENT_LIGHTMAPPED
            } else {
                0
            },
        lightmap: lightmap.unwrap_or(ContentHash::ZERO),
        uv_scale: if lightmap.is_some() { [1.0, 1.0] } else { [0.0; 2] },
        uv_offset: [0.0, 0.0],
    }
}

/// A three-sector strip (x = 0, 1, 2) in the cook's store layout; returns the public key.
fn world(root: &Path) -> Result<[u8; 32], Error> {
    world_with(root, None, false, 0)
}

/// The strip, optionally without the visual of sector `(skip, 0)` and with a visual for
/// a sector that has no gameplay container.
fn world_with(
    root: &Path,
    skip_visual: Option<i32>,
    orphan: bool,
    expected_flags: u32,
) -> Result<[u8; 32], Error> {
    let mut w = Writer {
        root: root.to_path_buf(),
        entries: [Vec::new(), Vec::new(), Vec::new()],
    };
    let (bv, bi) = mantis_render::mesh::cube();
    let box_mesh = mesh(convert(&bv), bi).encode();
    let box_hash = w.put(
        Domain::Presentation,
        "meshes/box.mesh",
        AssetKind::Mesh,
        &box_mesh,
    )?;
    let (gv, gi) = mantis_render::mesh::plane(SIZE);
    let ground = w.put(
        Domain::Presentation,
        "meshes/ground.mesh",
        AssetKind::Mesh,
        &mesh(convert(&gv), gi).encode(),
    )?;
    let texture = TextureAsset {
        encoding: Encoding::Rgba8,
        flags: 0,
        encoder_version: 0,
        width: 4,
        height: 4,
        mips: (0..full_chain(4, 4))
            .map(|l| {
                let (mw, mh) = mip_size(4, 4, l);
                vec![180; (mw * mh * 4) as usize]
            })
            .collect(),
    };
    let tex = w.put(
        Domain::Presentation,
        "textures/grey.tex",
        AssetKind::Texture,
        &texture.encode(),
    )?;
    let mat = w.put(
        Domain::Presentation,
        "materials/plain.mat",
        AssetKind::Material,
        &material(tex, expected_flags)?.encode(),
    )?;
    // A material no sector places (a character's, say): never the streamer's to compile.
    let mut unplaced = material(tex, expected_flags)?;
    unplaced.casts_shadows = !unplaced.casts_shadows;
    let _ = w.put(
        Domain::Presentation,
        "materials/unplaced.mat",
        AssetKind::Material,
        &unplaced.encode(),
    )?;
    for x in 0..3 {
        let lightmap = Lightmap {
            width: 8,
            height: 8,
            keyframes: vec![0.5],
            layers: vec![vec![rgb_to_rgb9e5([0.3; 3]); 64]],
        };
        let lm = w.put(
            Domain::Presentation,
            &format!("sectors/{x}_0.lightmap"),
            AssetKind::Lightmap,
            &lightmap.encode(),
        )?;
        let origin = x as f32 * SIZE;
        let probes = ProbeVolume {
            dims: [2, 2, 2],
            origin: [origin, 0.0, 0.0],
            spacing: [SIZE, 4.0, SIZE],
            keyframes: vec![0.5],
            probes: vec![vec![ShL1::constant([0.2; 3]); 8]],
            valid: None,
        };
        let pv = w.put(
            Domain::Presentation,
            &format!("sectors/{x}_0.probes"),
            AssetKind::ProbeVolume,
            &probes.encode(),
        )?;
        let sector = Sector {
            info: SectorInfo {
                sector_x: x,
                sector_z: 0,
                sector_size: SIZE,
                content_version: 0,
            },
            ground: Some(GroundGrid {
                origin_x: origin,
                origin_z: 0.0,
                cell_size: 4.0,
                width: 5,
                depth: 5,
                heights: vec![0.0; 25],
            }),
            hulls: Some(vec![ConvexHull {
                aabb_min: [origin + 2.0, 0.0, 2.0],
                aabb_max: [origin + 3.0, 2.0, 3.0],
                flags: HULL_BLOCKS_MOVEMENT,
                planes: vec![
                    [1.0, 0.0, 0.0, origin + 3.0],
                    [-1.0, 0.0, 0.0, -origin - 2.0],
                    [0.0, 1.0, 0.0, 2.0],
                    [0.0, -1.0, 0.0, 0.0],
                    [0.0, 0.0, 1.0, 3.0],
                    [0.0, 0.0, -1.0, -2.0],
                ],
            }]),
            triggers: None,
            placements: Some(vec![
                placement(ground, mat, [origin + 8.0, 0.0, 8.0], None),
                placement(box_hash, mat, [origin + 8.0, 0.5, 8.0], Some(lm)),
            ]),
            lightmaps: Some(vec![lm]),
            probe_volume: Some(pv),
            streaming: None,
        };
        let _ = w.put(
            Domain::Gameplay,
            &format!("sectors/{x}_0.sector"),
            AssetKind::Sector,
            &sector.for_client().gameplay().encode(),
        )?;
        if skip_visual != Some(x) {
            let _ = w.put(
                Domain::Presentation,
                &format!("sectors/{x}_0.visual"),
                AssetKind::Sector,
                &sector.visual().encode(),
            )?;
        }
        if orphan && x == 0 {
            let mut lost = sector.visual();
            lost.info.sector_x = 7;
            let _ = w.put(
                Domain::Presentation,
                "sectors/7_0.visual",
                AssetKind::Sector,
                &lost.encode(),
            )?;
        }
    }
    w.finish()
}

fn renderer(ctx: &GpuContext) -> Result<Renderer, Error> {
    let mut config = RendererConfig::new(64, 64, wgpu::TextureFormat::Rgba8Unorm);
    config.max_instances = 64;
    config.max_vertices = 1 << 14;
    config.max_indices = 1 << 15;
    config.shadow_resolution = 256;
    Ok(Renderer::new(&ctx.device, &ctx.queue, false, config)?)
}

fn streaming() -> StreamingConfig {
    StreamingConfig {
        load_radius: 20.0,
        unload_radius: 28.0,
        lookahead: 0.0,
        ..StreamingConfig::default()
    }
}

/// Updates until nothing is in flight or backlogged (workers run in real time).
fn settle(w: &mut WorldStreamer, r: &mut Renderer, gpu: &Gpu<'_>, at: Vec3) -> Result<WorldStats, Error> {
    let clock = ManualClock::new();
    let mut last = WorldStats::default();
    for _ in 0..2000 {
        last = w.update(
            r,
            gpu,
            &clock,
            Duration::from_millis(2),
            (at, Vec3::ZERO, Vec3::Z),
        );
        if last.stream.in_flight == 0 && !last.backlog && last.stream.requested == 0 {
            return Ok(last);
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    Err(format!("streaming did not settle: {last:?}").into())
}

fn frame(r: &mut Renderer, ctx: &GpuContext, at: Vec3) -> Result<(), Error> {
    let camera = Camera {
        position: at + Vec3::new(0.0, 6.0, -6.0),
        yaw: 0.0,
        pitch: -0.6,
        fov_y: 1.0,
        aspect: 1.0,
        near: 0.1,
    };
    let _ = r.prepare(&FrameInputs {
        camera,
        time: 0.0,
        sun_direction: Vec3::new(0.3, -1.0, 0.2),
        sun_color: Vec3::ONE,
        shadows: true,
        sky: ShL1::ZERO,
        ambient_intensity: 1.0,
        exposure: 1.0,
        clear_color: [0.0, 0.0, 0.0, 1.0],
        ssao_strength: 0.5,
        post: PostSettings::OFF,
    });
    let t = ctx.device.create_texture(&wgpu::TextureDescriptor {
        label: None,
        size: wgpu::Extent3d {
            width: 64,
            height: 64,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8Unorm,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
        view_formats: &[],
    });
    let v = t.create_view(&wgpu::TextureViewDescriptor::default());
    let mut encoder = ctx
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
    r.encode(&ctx.device, &ctx.queue, &mut encoder, (&t, &v))?;
    let _ = ctx.queue.submit(Some(encoder.finish()));
    Ok(())
}

fn gpu(ctx: &GpuContext) -> Gpu<'_> {
    Gpu {
        device: &ctx.device,
        queue: &ctx.queue,
        capabilities: ctx.capabilities,
    }
}

#[test]
fn sectors_stream_in_around_the_camera_and_out_behind_it() -> TestResult {
    let dir = TempDir::new("world-stream")?;
    let key = world(&dir.0)?;
    let store = ContentStore::open(&dir.0);
    let gameplay = store.bundle(Domain::Gameplay, &key)?;
    let presentation = store.bundle(Domain::Presentation, &key)?;
    let index = WorldIndex::build(&store, &gameplay, &presentation)?;
    assert_eq!(index.sectors.len(), 3);
    assert_eq!(index.sector_size, SIZE);
    assert_eq!(index.materials.len(), 1, "only the placed material is preloaded");
    let ctx = noop()?;
    let gpu = gpu(&ctx);
    let mut result: Result<(), Error> = Ok(());
    let errors = validation_errors(&ctx.device, || {
        result = (|| {
            let mut r = renderer(&ctx)?;
            let mut w = WorldStreamer::new(store.clone(), index.clone(), streaming(), 2)?;
            assert_eq!(w.preload_materials(&mut r, &gpu)?, 1, "compiled at load");
            assert_eq!(w.preload_materials(&mut r, &gpu)?, 0, "and only once");
            let start = Vec3::new(8.0, 0.0, 8.0);
            let stats = settle(&mut w, &mut r, &gpu, start)?;
            assert_eq!(w.take_errors(), Vec::new());
            assert_eq!(w.resident(), vec![(0, 0), (1, 0)], "{stats:?}");
            assert_eq!(stats.instances, 4, "ground and box per sector");
            assert_eq!(r.world_light().resident_pages(), 2);
            assert_eq!(r.world_light().resident_sectors(), 2);
            assert_eq!(r.scene_mut().instance_count(), 4);
            frame(&mut r, &ctx, start)?;
            assert_eq!(r.stats().batches, 2, "probe-lit ground and lightmapped boxes");
            // Walk to the far end: the first sector unloads, the last loads.
            let end = Vec3::new(40.0, 0.0, 8.0);
            let _ = settle(&mut w, &mut r, &gpu, end)?;
            assert_eq!(w.resident(), vec![(1, 0), (2, 0)]);
            assert_eq!(r.scene_mut().instance_count(), 4);
            assert_eq!(r.world_light().resident_pages(), 2, "pages released and reused");
            frame(&mut r, &ctx, end)?;
            assert_eq!(w.take_errors(), Vec::new());
            // A restart (the renderer was rebuilt) streams everything again from nothing.
            let mut fresh = renderer(&ctx)?;
            w.restart();
            let _ = settle(&mut w, &mut fresh, &gpu, end)?;
            assert_eq!(w.resident(), vec![(1, 0), (2, 0)]);
            assert_eq!(fresh.scene_mut().instance_count(), 4);
            Ok(())
        })();
    });
    result?;
    assert_eq!(errors, None);
    Ok(())
}

#[test]
fn tampered_objects_and_wrong_keys_fail_closed() -> TestResult {
    let dir = TempDir::new("world-tamper")?;
    let key = world(&dir.0)?;
    let store = ContentStore::open(&dir.0);
    let mut wrong = key;
    wrong[0] ^= 1;
    assert_eq!(
        store.bundle(Domain::Gameplay, &wrong).err(),
        Some(StoreError::Signature)
    );
    let gameplay = store.bundle(Domain::Gameplay, &key)?;
    let presentation = store.bundle(Domain::Presentation, &key)?;
    let index = WorldIndex::build(&store, &gameplay, &presentation)?;
    // Corrupt the probe volume of sector (0, 0): its load must fail and leave nothing of
    // the sector in the renderer.
    let probes = presentation.get("sectors/0_0.probes").ok_or("probes entry")?.hash;
    let hex = probes.to_string();
    let path = dir.0.join("objects").join(hex.get(..2).ok_or("hex")?).join(&hex);
    let mut bytes = std::fs::read(&path)?;
    if let Some(b) = bytes.get_mut(40) {
        *b ^= 0xff;
    }
    std::fs::write(&path, bytes)?;
    assert_eq!(store.get(&probes).err(), Some(StoreError::Corrupt(probes)));
    let ctx = noop()?;
    let gpu = gpu(&ctx);
    let mut r = renderer(&ctx)?;
    let mut w = WorldStreamer::new(store, index, streaming(), 1)?;
    let clock = ManualClock::new();
    let at = Vec3::new(8.0, 0.0, 8.0);
    let mut failures = 0;
    for _ in 0..2000 {
        let s = w.update(
            &mut r,
            &gpu,
            &clock,
            Duration::from_millis(2),
            (at, Vec3::ZERO, Vec3::Z),
        );
        failures += s.failed;
        if failures > 0 && w.resident() == vec![(1, 0)] {
            break;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(failures > 0);
    assert_eq!(w.resident(), vec![(1, 0)], "the intact neighbor still streams");
    let errors = w.take_errors();
    assert!(
        errors.contains(&WorldError::Store(StoreError::Corrupt(probes))),
        "{errors:?}"
    );
    assert_eq!(r.world_light().resident_sectors(), 1);
    assert_eq!(r.scene_mut().instance_count(), 2);
    Ok(())
}

#[test]
fn sectors_without_visuals_draw_their_collision_and_orphan_visuals_are_refused() -> TestResult {
    let dir = TempDir::new("world-collision")?;
    let key = world_with(&dir.0, Some(1), false, 0)?;
    let store = ContentStore::open(&dir.0);
    let gameplay = store.bundle(Domain::Gameplay, &key)?;
    let presentation = store.bundle(Domain::Presentation, &key)?;
    let index = WorldIndex::build(&store, &gameplay, &presentation)?;
    assert_eq!(index.collision_only.len(), 1, "sector (1, 0) has no visual");
    let ctx = noop()?;
    let gpu = gpu(&ctx);
    let start = Vec3::new(8.0, 0.0, 8.0);
    for (debug, instances) in [(true, 2 + 2), (false, 2)] {
        let mut r = renderer(&ctx)?;
        let mut w = WorldStreamer::new(store.clone(), index.clone(), streaming(), 1)?;
        w.set_debug_collision(debug);
        let settled = settle(&mut w, &mut r, &gpu, start);
        assert_eq!(w.take_errors(), Vec::new());
        let _ = settled?;
        assert_eq!(
            w.resident(),
            vec![(0, 0), (1, 0)],
            "collision-only sectors stream too"
        );
        assert_eq!(
            r.scene_mut().instance_count(),
            instances,
            "debug {debug}: sector (1, 0) draws its ground and hull, or nothing"
        );
        assert_eq!(
            r.world_light().resident_sectors(),
            1,
            "no probes without a visual"
        );
        frame(&mut r, &ctx, start)?;
    }
    let orphan = TempDir::new("world-orphan")?;
    let key = world_with(&orphan.0, None, true, 0)?;
    let store = ContentStore::open(&orphan.0);
    let refused = WorldIndex::build(
        &store,
        &store.bundle(Domain::Gameplay, &key)?,
        &store.bundle(Domain::Presentation, &key)?,
    );
    assert_eq!(refused.err(), Some(WorldError::OrphanVisual((7, 0))));
    Ok(())
}

#[test]
fn a_texture_unlike_its_material_expects_is_refused_at_load() -> TestResult {
    // The material expects an sRGB texture; the stored one is linear.
    let dir = TempDir::new("world-flags")?;
    let key = world_with(&dir.0, None, false, mantis_formats::material::TEXTURE_SRGB)?;
    let store = ContentStore::open(&dir.0);
    let index = WorldIndex::build(
        &store,
        &store.bundle(Domain::Gameplay, &key)?,
        &store.bundle(Domain::Presentation, &key)?,
    )?;
    let ctx = noop()?;
    let gpu = gpu(&ctx);
    let mut r = renderer(&ctx)?;
    let mut w = WorldStreamer::new(store, index, streaming(), 1)?;
    let refused = w.preload_materials(&mut r, &gpu);
    assert!(
        matches!(&refused, Err(WorldError::Format { error, .. }) if error.contains("expects")),
        "{refused:?}"
    );
    Ok(())
}

#[test]
fn unused_assets_are_evicted_and_stream_back() -> TestResult {
    let dir = TempDir::new("world-evict")?;
    let key = world(&dir.0)?;
    let store = ContentStore::open(&dir.0);
    let index = WorldIndex::build(
        &store,
        &store.bundle(Domain::Gameplay, &key)?,
        &store.bundle(Domain::Presentation, &key)?,
    )?;
    let ctx = noop()?;
    let gpu = gpu(&ctx);
    let mut r = renderer(&ctx)?;
    let empty = r.mesh_space();
    let mut w = WorldStreamer::new(store, index, streaming(), 1)?;
    w.set_cache_budget(0);
    let near = Vec3::new(8.0, 0.0, 8.0);
    let far = Vec3::new(500.0, 0.0, 8.0);
    let mut peak = None;
    for lap in 0..3 {
        let _ = settle(&mut w, &mut r, &gpu, near)?;
        let loaded = w.residency();
        assert_eq!(
            (loaded.sectors, loaded.meshes, loaded.materials, loaded.textures),
            (2, 2, 1, 1),
            "lap {lap}: two sectors share the box and ground meshes, one material, one texture"
        );
        let counts = (
            loaded.sectors,
            loaded.meshes,
            loaded.materials,
            loaded.textures,
            loaded.unreferenced,
        );
        assert!(peak.is_none_or(|p| p == counts), "lap {lap}: residency is flat");
        peak = Some(counts);
        let _ = settle(&mut w, &mut r, &gpu, far)?;
        let away = w.residency();
        assert_eq!(
            (
                away.sectors,
                away.meshes,
                away.materials,
                away.textures,
                away.unreferenced
            ),
            (0, 0, 0, 0, 0),
            "lap {lap}: everything evicted"
        );
        assert_eq!(r.mesh_space(), empty, "lap {lap}: mesh ranges returned");
        assert_eq!(r.materials().len(), 0);
        assert_eq!(w.take_errors(), Vec::new());
    }
    assert_eq!(
        w.residency().evicted,
        3 * 4,
        "box, ground, material, texture per lap"
    );
    // With a budget, unused assets stay loaded for a sector streaming back.
    w.set_cache_budget(8);
    let _ = settle(&mut w, &mut r, &gpu, near)?;
    let _ = settle(&mut w, &mut r, &gpu, far)?;
    let kept = w.residency();
    assert_eq!((kept.sectors, kept.meshes, kept.unreferenced), (0, 2, 3));
    Ok(())
}

/// A unit cube with every face split into `n x n` quads: the same surface as
/// `mantis_render::mesh::cube`, with more triangles (a finer level of detail).
fn subdivided_cube(cuts: u32) -> (Vec<mantis_render::gpu_types::Vertex>, Vec<u32>) {
    let (cube_vertices, cube_indices) = mantis_render::mesh::cube();
    let mut vertices = Vec::new();
    let mut indices = Vec::new();
    let Some(first) = cube_vertices.first().copied() else {
        return (vertices, indices);
    };
    let lerp = |from: [f32; 3], to: [f32; 3], t: f32| [0, 1, 2].map(|k| from[k] + (to[k] - from[k]) * t);
    for face in cube_indices.as_chunks::<6>().0 {
        // The cube's faces are (a, b, c, a, c, d).
        let [corner_a, corner_b, corner_c, _, _, corner_d] =
            face.map(|k| cube_vertices.get(k as usize).copied().unwrap_or(first));
        let base = u32::try_from(vertices.len()).unwrap_or(0);
        for row in 0..=cuts {
            for col in 0..=cuts {
                let (across, down) = (col as f32 / cuts as f32, row as f32 / cuts as f32);
                let top = lerp(corner_a.position, corner_b.position, across);
                let bottom = lerp(corner_d.position, corner_c.position, across);
                let mut vertex = corner_a;
                vertex.position = lerp(top, bottom, down);
                vertices.push(vertex);
            }
        }
        for row in 0..cuts {
            for col in 0..cuts {
                let here = base + row * (cuts + 1) + col;
                let (right, below, diagonal) = (here + 1, here + cuts + 1, here + cuts + 2);
                indices.extend_from_slice(&[here, right, diagonal, here, diagonal, below]);
            }
        }
    }
    (vertices, indices)
}

/// One sector with one placement of three levels: within 6 m a subdivided cube, within
/// 12 m and 24 m plain cubes (two distinct meshes). Lit by probes and the sun.
fn lod_world(root: &Path) -> Result<[u8; 32], Error> {
    let mut w = Writer {
        root: root.to_path_buf(),
        entries: [Vec::new(), Vec::new(), Vec::new()],
    };
    let (fv, fi) = subdivided_cube(3);
    let (cv, ci) = mantis_render::mesh::cube();
    let fine = w.put(
        Domain::Presentation,
        "meshes/fine.mesh",
        AssetKind::Mesh,
        &mesh(convert(&fv), fi).encode(),
    )?;
    let mid = w.put(
        Domain::Presentation,
        "meshes/mid.mesh",
        AssetKind::Mesh,
        &mesh(convert(&cv), ci.clone()).encode(),
    )?;
    // The far level differs in its second uv set only: a distinct mesh, the same surface.
    let mut far_vertices = convert(&cv);
    for v in &mut far_vertices {
        v.uv1 = [0.5, 0.5];
    }
    let far = w.put(
        Domain::Presentation,
        "meshes/far.mesh",
        AssetKind::Mesh,
        &mesh(far_vertices, ci).encode(),
    )?;
    let texture = TextureAsset {
        encoding: Encoding::Rgba8,
        flags: 0,
        encoder_version: 0,
        width: 1,
        height: 1,
        mips: vec![vec![200; 4]],
    };
    let tex = w.put(
        Domain::Presentation,
        "textures/grey.tex",
        AssetKind::Texture,
        &texture.encode(),
    )?;
    let mat = w.put(
        Domain::Presentation,
        "materials/plain.mat",
        AssetKind::Material,
        &material(tex, 0)?.encode(),
    )?;
    let probes = ProbeVolume {
        dims: [2, 2, 2],
        origin: [0.0, 0.0, 0.0],
        spacing: [SIZE, 4.0, SIZE],
        keyframes: vec![0.5],
        probes: vec![vec![ShL1::constant([0.4; 3]); 8]],
        valid: None,
    };
    let pv = w.put(
        Domain::Presentation,
        "sectors/0_0.probes",
        AssetKind::ProbeVolume,
        &probes.encode(),
    )?;
    let mut lod = placement(fine, mat, [8.0, 0.0, 8.0], None);
    lod.lod_count = 3;
    lod.lod_ranges = [6.0, 12.0, 24.0, 0.0];
    lod.lod_meshes = [mid, far, ContentHash::ZERO];
    let sector = Sector {
        info: SectorInfo {
            sector_x: 0,
            sector_z: 0,
            sector_size: SIZE,
            content_version: 0,
        },
        ground: None,
        hulls: None,
        triggers: None,
        placements: Some(vec![lod]),
        lightmaps: None,
        probe_volume: Some(pv),
        streaming: None,
    };
    let _ = w.put(
        Domain::Gameplay,
        "sectors/0_0.sector",
        AssetKind::Sector,
        &sector.gameplay().encode(),
    )?;
    let _ = w.put(
        Domain::Presentation,
        "sectors/0_0.visual",
        AssetKind::Sector,
        &sector.visual().encode(),
    )?;
    w.finish()
}

fn draw_at(r: &mut Renderer, ctx: &GpuContext, eye: Vec3) -> Result<Vec<u8>, Error> {
    let camera = Camera {
        position: eye,
        yaw: 0.0,
        pitch: 0.0,
        fov_y: 1.0,
        aspect: 1.0,
        near: 0.1,
    };
    let _ = r.prepare(&FrameInputs {
        camera,
        time: 0.0,
        sun_direction: Vec3::new(0.3, -1.0, 0.6),
        sun_color: Vec3::ONE,
        shadows: false,
        sky: ShL1::ZERO,
        ambient_intensity: 1.0,
        exposure: 1.0,
        clear_color: [0.0, 0.0, 0.0, 1.0],
        ssao_strength: 0.0,
        post: PostSettings::OFF,
    });
    let t = ctx.device.create_texture(&wgpu::TextureDescriptor {
        label: None,
        size: wgpu::Extent3d {
            width: 64,
            height: 64,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8Unorm,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let v = t.create_view(&wgpu::TextureViewDescriptor::default());
    let mut encoder = ctx
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
    r.encode(&ctx.device, &ctx.queue, &mut encoder, (&t, &v))?;
    let _ = ctx.queue.submit(Some(encoder.finish()));
    if ctx.is_noop {
        return Ok(Vec::new());
    }
    Ok(mantis_render::gpu_test::read_texture_4bpp(ctx, &t)?)
}

#[test]
fn walking_toward_a_placement_steps_through_its_levels_without_a_pop() -> TestResult {
    let dir = TempDir::new("world-lod")?;
    let key = lod_world(&dir.0)?;
    let store = ContentStore::open(&dir.0);
    let index = WorldIndex::build(
        &store,
        &store.bundle(Domain::Gameplay, &key)?,
        &store.bundle(Domain::Presentation, &key)?,
    )?;
    let hardware = mantis_render::gpu_test::hardware_or_skip(
        "walking_toward_a_placement_steps_through_its_levels_without_a_pop (pixels)",
    );
    let pixels = hardware.is_some();
    let ctx = match hardware {
        Some(c) => c,
        None => noop()?,
    };
    let gpu = gpu(&ctx);
    let mut r = renderer(&ctx)?;
    let config = StreamingConfig {
        load_radius: 60.0,
        unload_radius: 80.0,
        lookahead: 0.0,
        ..StreamingConfig::default()
    };
    let mut w = WorldStreamer::new(store, index, config, 1)?;
    let target = Vec3::new(8.0, 0.0, 8.0);
    let eye_at = |d: f32| target + Vec3::new(0.0, 0.0, -d);
    let clock = ManualClock::new();
    // Stream the sector in from 30 m (beyond the last range).
    let _ = settle(&mut w, &mut r, &gpu, eye_at(30.0))?;
    assert_eq!(
        w.placement_levels(),
        vec![None],
        "beyond the last range: no instance"
    );
    let mut seen = vec![None];
    let mut worst = 0.0f64;
    let mut d = 30.0f32;
    while d > 2.0 {
        d -= 0.25;
        let eye = eye_at(d);
        // The frame before the update still draws the previous level; the frame after
        // draws the new one, from the same eye.
        let before = draw_at(&mut r, &ctx, eye)?;
        let _ = w.update(
            &mut r,
            &gpu,
            &clock,
            Duration::from_millis(2),
            (eye, Vec3::ZERO, Vec3::Z),
        );
        let level = w.placement_levels().first().copied().flatten();
        if seen.last() != Some(&level) {
            seen.push(level);
            let after = draw_at(&mut r, &ctx, eye)?;
            if pixels && before.len() == after.len() && level.is_some_and(|l| l < 2) {
                let differing = before
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .zip(after.as_chunks::<4>().0)
                    .filter(|(a, b)| a.iter().zip(b.iter()).any(|(x, y)| x.abs_diff(*y) > 8))
                    .count();
                let fraction = differing as f64 / (before.len() / 4) as f64;
                worst = worst.max(fraction);
                assert!(
                    fraction < 0.005,
                    "level switch at {d} m changed {differing} pixels"
                );
            }
        }
    }
    assert_eq!(
        seen,
        vec![None, Some(2), Some(1), Some(0)],
        "every level in order, once"
    );
    // Hysteresis: hovering around the 12 m switch from inside does not flicker.
    let mut changes = 0;
    for k in 0..20 {
        let d = if k % 2 == 0 { 11.5 } else { 12.6 };
        let eye = eye_at(d);
        changes += w
            .update(
                &mut r,
                &gpu,
                &clock,
                Duration::from_millis(2),
                (eye, Vec3::ZERO, Vec3::Z),
            )
            .lod_changes;
    }
    // One change back to level 1 (from level 0), then none while hovering.
    assert_eq!(changes, 1, "no flicker at the boundary");
    assert_eq!(w.placement_levels(), vec![Some(1)]);
    assert_eq!(w.take_errors(), Vec::new());
    if pixels {
        println!("MANTIS-METRIC client_lod_switch_pixels worst_fraction={worst:.5} target=0.005");
    } else {
        println!(
            "{}: lod switch pixels (no-op backend; structure asserted)",
            mantis_render::gpu_test::SKIP_MARKER
        );
    }
    Ok(())
}

#[test]
fn level_selection_has_hysteresis_both_ways() {
    use mantis_client::world_stream::select_lod;
    let ranges = [10.0, 20.0];
    assert_eq!(select_lod(None, true, 5.0, &ranges), Some(0));
    assert_eq!(select_lod(None, true, 25.0, &ranges), None);
    // Coarser only past 110 % of the current range; finer only inside 90 % of the target.
    assert_eq!(select_lod(Some(0), false, 10.5, &ranges), Some(0));
    assert_eq!(select_lod(Some(0), false, 11.5, &ranges), Some(1));
    assert_eq!(select_lod(Some(1), false, 9.5, &ranges), Some(1));
    assert_eq!(select_lod(Some(1), false, 8.5, &ranges), Some(0));
    assert_eq!(select_lod(Some(1), false, 21.0, &ranges), Some(1));
    assert_eq!(select_lod(Some(1), false, 23.0, &ranges), None);
    assert_eq!(select_lod(None, false, 19.0, &ranges), None);
    assert_eq!(select_lod(None, false, 17.0, &ranges), Some(1));
}

#[test]
fn release_all_returns_every_renderer_range_and_streams_again() -> TestResult {
    let dir = TempDir::new("world-release")?;
    let key = world(&dir.0)?;
    let store = ContentStore::open(&dir.0);
    let index = WorldIndex::build(
        &store,
        &store.bundle(Domain::Gameplay, &key)?,
        &store.bundle(Domain::Presentation, &key)?,
    )?;
    let ctx = noop()?;
    let gpu = gpu(&ctx);
    let mut r = renderer(&ctx)?;
    let empty = r.mesh_space();
    let materials_before = r.materials().len();
    let mut w = WorldStreamer::new(store, index, streaming(), 1)?;
    assert_eq!(w.preload_materials(&mut r, &gpu)?, 1);
    let near = Vec3::new(8.0, 0.0, 8.0);
    for round in 0..2 {
        let _ = settle(&mut w, &mut r, &gpu, near)?;
        assert_eq!(w.residency().sectors, 2, "round {round}");
        let released = w.release_all(&mut r, &gpu);
        // Two meshes, the pinned material, and its texture.
        assert_eq!(released, 4, "round {round}");
        assert_eq!(
            w.residency(),
            Residency {
                evicted: w.residency().evicted,
                ..Residency::default()
            }
        );
        assert_eq!(r.mesh_space(), empty, "every mesh range returned");
        assert_eq!(r.materials().len(), materials_before, "every material removed");
        assert_eq!(w.take_errors(), Vec::new());
    }
    // A released streamer streams in again (materials compile on demand now).
    let _ = settle(&mut w, &mut r, &gpu, near)?;
    assert_eq!(w.residency().sectors, 2);
    Ok(())
}
