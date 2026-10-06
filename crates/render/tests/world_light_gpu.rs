//! Streamed indirect light: lightmap pages and the probe atlas with its toroidal sector
//! table. Loading rules run on the no-op backend; pixel checks run on a real headless
//! adapter (skipped and counted when none exists).

#![expect(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::too_many_lines
)] // Pixel math in tests.

use glam::{Mat4, Vec3};
use mantis_formats::lightmap::{Lightmap, rgb_to_rgb9e5};
use mantis_formats::material::{
    Deformations, LightingModel, MaterialAsset, MaterialGraph, Node, NodeId, SurfaceOutputs,
};
use mantis_formats::probe_volume::ProbeVolume;
use mantis_formats::sh::ShL1;
use mantis_render::gpu::GpuContext;
use mantis_render::gpu_test::{hardware_or_skip, noop, read_texture_4bpp, validation_errors};
use mantis_render::math::Camera;
use mantis_render::mesh::plane;
use mantis_render::post::PostSettings;
use mantis_render::renderer::{FrameInputs, Renderer, RendererConfig};
use mantis_render::scene::{MaterialId, MeshId};
use mantis_render::world_light::{WorldLightConfig, WorldLightError};

type TestResult = Result<(), Box<dyn std::error::Error>>;

const SIZE: u32 = 64;
const OUTPUT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;
const SECTOR: f32 = 10.0;

fn white() -> Result<MaterialAsset, Box<dyn std::error::Error>> {
    let graph = MaterialGraph {
        nodes: vec![Node::ColorParam(0), Node::Swizzle(NodeId(0), [0, 1, 2, 0], 3)],
        outputs: SurfaceOutputs {
            base_color: NodeId(1),
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
        deformations: Deformations::ALL,
        bindings: mantis_formats::material::MaterialBindings {
            textures: Vec::new(),
            scalars: Vec::new(),
            colors: vec![mantis_formats::material::ColorDefault {
                name: "color".to_owned(),
                value: [1.0, 1.0, 1.0, 1.0],
            }],
        },
    })
}

struct Rig {
    renderer: Renderer,
    plane: MeshId,
    material: MaterialId,
}

fn rig(ctx: &GpuContext, world_light: WorldLightConfig) -> Result<Rig, Box<dyn std::error::Error>> {
    let mut config = RendererConfig::new(SIZE, SIZE, OUTPUT);
    config.max_instances = 64;
    config.max_batches = 16;
    config.max_vertices = 1024;
    config.max_indices = 2048;
    config.shadow_resolution = 256;
    config.world_light = world_light;
    let mut renderer = Renderer::new(&ctx.device, &ctx.queue, false, config)?;
    let (pv, pi) = plane(1.0);
    let plane = renderer.add_mesh(&ctx.queue, &pv, &pi)?;
    let material = renderer.add_material(&ctx.device, &ctx.queue, white()?, [None; 4])?;
    Ok(Rig {
        renderer,
        plane,
        material,
    })
}

fn small() -> WorldLightConfig {
    WorldLightConfig {
        page_size: 8,
        page_layers: 6,
        pages: 3,
        brick: [2, 2, 2],
        bricks: [2, 1],
        table_side: 4,
    }
}

/// Looking straight down at the ground around `center` (x, z), 24 m across.
fn top_camera(center: Vec3) -> Camera {
    Camera {
        position: center + Vec3::new(0.0, 20.0, 0.0),
        yaw: 0.0,
        pitch: -1.5,
        fov_y: 1.2,
        aspect: 1.0,
        near: 0.1,
    }
}

/// Ambient light only: no sun, no sky, no occlusion, no post.
fn inputs(camera: Camera) -> FrameInputs {
    FrameInputs {
        camera,
        time: 0.0,
        sun_direction: Vec3::NEG_Y,
        sun_color: Vec3::ZERO,
        shadows: false,
        sky: ShL1::ZERO,
        ambient_intensity: 1.0,
        exposure: 1.0,
        clear_color: [0.0, 0.0, 0.0, 1.0],
        ssao_strength: 0.0,
        post: PostSettings::OFF,
    }
}

fn render(
    ctx: &GpuContext,
    r: &mut Renderer,
    frame: &FrameInputs,
) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let t = encode_frame(ctx, r, frame)?;
    Ok(read_texture_4bpp(ctx, &t)?)
}

fn encode_frame(
    ctx: &GpuContext,
    r: &mut Renderer,
    frame: &FrameInputs,
) -> Result<wgpu::Texture, Box<dyn std::error::Error>> {
    let t = ctx.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("output"),
        size: wgpu::Extent3d {
            width: SIZE,
            height: SIZE,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: OUTPUT,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let v = t.create_view(&wgpu::TextureViewDescriptor::default());
    let _ = r.prepare(frame);
    let mut encoder = ctx
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("frame") });
    r.encode(&ctx.device, &ctx.queue, &mut encoder, (&t, &v))?;
    let _ = ctx.queue.submit(Some(encoder.finish()));
    Ok(t)
}

fn rgb_at(px: &[u8], camera: &Camera, p: Vec3) -> [u8; 3] {
    let clip = camera.view_projection() * p.extend(1.0);
    let ndc = clip.truncate() / clip.w;
    let x = ((ndc.x * 0.5 + 0.5) * SIZE as f32) as u32;
    let y = ((0.5 - ndc.y * 0.5) * SIZE as f32) as u32;
    let i = ((y.min(SIZE - 1) * SIZE + x.min(SIZE - 1)) * 4) as usize;
    px.get(i..i + 3).map_or([0; 3], |s| {
        [
            s.first().copied().unwrap_or(0),
            s.get(1).copied().unwrap_or(0),
            s.get(2).copied().unwrap_or(0),
        ]
    })
}

/// The renderer's ACES curve on the CPU, to 8 bits.
fn aces8(x: f32) -> u8 {
    let y = (x * (2.51 * x + 0.03)) / (x * (2.43 * x + 0.59) + 0.14);
    (y.clamp(0.0, 1.0) * 255.0).round() as u8
}

fn near(a: [u8; 3], b: [u8; 3]) -> bool {
    a.iter().zip(&b).all(|(x, y)| x.abs_diff(*y) <= 3)
}

/// A 2 x 2 x 2 volume spanning sector `(x, z)` with one constant light per keyframe.
fn volume(x: i32, z: i32, keys: &[(f32, [f32; 3])]) -> ProbeVolume {
    ProbeVolume {
        dims: [2, 2, 2],
        origin: [x as f32 * SECTOR, -1.0, z as f32 * SECTOR],
        spacing: [SECTOR, 2.0, SECTOR],
        keyframes: keys.iter().map(|k| k.0).collect(),
        probes: keys.iter().map(|k| vec![ShL1::constant(k.1); 8]).collect(),
        valid: None,
    }
}

fn solid_lightmap(width: u32, height: u32, keys: &[(f32, [f32; 3])]) -> Lightmap {
    Lightmap {
        width,
        height,
        keyframes: keys.iter().map(|k| k.0).collect(),
        layers: keys
            .iter()
            .map(|k| vec![rgb_to_rgb9e5(k.1); (width * height) as usize])
            .collect(),
    }
}

#[test]
fn probes_light_each_sector_from_its_own_brick_and_fall_back_to_the_sky() -> TestResult {
    let Some(ctx) = hardware_or_skip("probes_light_each_sector_from_its_own_brick_and_fall_back_to_the_sky")
    else {
        return Ok(());
    };
    let mut r = rig(&ctx, small())?;
    let ground = Mat4::from_scale_rotation_translation(Vec3::splat(40.0), glam::Quat::IDENTITY, Vec3::ZERO);
    let _ = r.renderer.scene_mut().spawn(r.plane, r.material, ground)?;
    let red = [0.5, 0.0, 0.0];
    let green = [0.0, 0.5, 0.0];
    let blue = [0.0, 0.0, 0.5];
    r.renderer.world_light_mut().add_probes(
        &ctx.queue,
        (0, 0),
        SECTOR,
        volume(0, 0, &[(0.0, red), (0.5, blue)]),
    )?;
    r.renderer
        .world_light_mut()
        .add_probes(&ctx.queue, (1, 0), SECTOR, volume(1, 0, &[(0.0, green)]))?;
    let camera = top_camera(Vec3::new(5.0, 0.0, 5.0));
    let frame = inputs(camera);
    let lit = aces8(0.5);
    let in_a = Vec3::new(5.0, 0.0, 5.0);
    let in_b = Vec3::new(13.0, 0.0, 5.0);
    let outside = Vec3::new(-3.0, 0.0, 5.0);
    let px = render(&ctx, &mut r.renderer, &frame)?;
    assert!(
        near(rgb_at(&px, &camera, in_a), [lit, 0, 0]),
        "sector (0, 0) red: {:?}",
        rgb_at(&px, &camera, in_a)
    );
    assert!(
        near(rgb_at(&px, &camera, in_b), [0, lit, 0]),
        "sector (1, 0) green: {:?}",
        rgb_at(&px, &camera, in_b)
    );
    assert!(
        near(rgb_at(&px, &camera, outside), [0, 0, 0]),
        "unloaded sector falls back to the black sky"
    );
    // Time of day re-blends the brick; unloading a sector returns it to the sky.
    r.renderer.world_light_mut().set_time(&ctx.queue, 0.5);
    r.renderer.world_light_mut().remove_probes((1, 0))?;
    let px = render(&ctx, &mut r.renderer, &frame)?;
    assert!(
        near(rgb_at(&px, &camera, in_a), [0, 0, lit]),
        "keyframe 0.5 is blue: {:?}",
        rgb_at(&px, &camera, in_a)
    );
    assert!(
        near(rgb_at(&px, &camera, in_b), [0, 0, 0]),
        "unloaded sector is sky"
    );
    // A sector whose coordinates wrap onto a freed table slot loads into it.
    r.renderer
        .world_light_mut()
        .add_probes(&ctx.queue, (-3, 0), SECTOR, volume(-3, 0, &[(0.0, green)]))?;
    let px = render(&ctx, &mut r.renderer, &frame)?;
    assert!(
        near(rgb_at(&px, &camera, in_b), [0, 0, 0]),
        "the wrapped slot names (-3, 0), not (1, 0)"
    );
    Ok(())
}

#[test]
fn lightmapped_instances_read_their_own_page_rectangle_and_blend_keyframes() -> TestResult {
    let Some(ctx) =
        hardware_or_skip("lightmapped_instances_read_their_own_page_rectangle_and_blend_keyframes")
    else {
        return Ok(());
    };
    let mut r = rig(&ctx, small())?;
    let red = [0.5, 0.0, 0.0];
    let blue = [0.0, 0.0, 0.5];
    let green = [0.0, 0.5, 0.0];
    let a = r
        .renderer
        .world_light_mut()
        .add_lightmap(&ctx.queue, &solid_lightmap(2, 2, &[(0.0, red), (0.5, blue)]))?;
    // A 4 x 2 atlas: left half green, right half white.
    let mut b_map = solid_lightmap(4, 2, &[(0.0, [1.0; 3])]);
    for row in 0..2 {
        for col in 0..2 {
            if let Some(t) = b_map.layers.first_mut().and_then(|l| l.get_mut(row * 4 + col)) {
                *t = rgb_to_rgb9e5(green);
            }
        }
    }
    let b = r.renderer.world_light_mut().add_lightmap(&ctx.queue, &b_map)?;
    let tile = |x: f32| {
        Mat4::from_scale_rotation_translation(Vec3::splat(4.0), glam::Quat::IDENTITY, Vec3::new(x, 0.0, 0.0))
    };
    let _ = r
        .renderer
        .spawn_lightmapped(r.plane, r.material, tile(-3.0), a, [1.0, 1.0], [0.0, 0.0])?;
    let _ = r
        .renderer
        .spawn_lightmapped(r.plane, r.material, tile(3.0), b, [0.5, 1.0], [0.0, 0.0])?;
    let camera = top_camera(Vec3::ZERO);
    let frame = inputs(camera);
    let lit = aces8(0.5);
    let (pa, pb) = (Vec3::new(-3.0, 0.0, 0.0), Vec3::new(3.0, 0.0, 0.0));
    let px = render(&ctx, &mut r.renderer, &frame)?;
    assert!(
        near(rgb_at(&px, &camera, pa), [lit, 0, 0]),
        "page a: {:?}",
        rgb_at(&px, &camera, pa)
    );
    assert!(
        near(rgb_at(&px, &camera, pb), [0, lit, 0]),
        "page b's left half: {:?}",
        rgb_at(&px, &camera, pb)
    );
    assert_eq!(
        r.renderer.stats().batches,
        1,
        "both pages share one lightmapped batch"
    );
    r.renderer.world_light_mut().set_time(&ctx.queue, 0.5);
    let px = render(&ctx, &mut r.renderer, &frame)?;
    assert!(
        near(rgb_at(&px, &camera, pa), [0, 0, lit]),
        "page a at 0.5: {:?}",
        rgb_at(&px, &camera, pa)
    );
    assert!(
        near(rgb_at(&px, &camera, pb), [0, lit, 0]),
        "single-keyframe page b is unchanged"
    );
    Ok(())
}

#[test]
fn loading_rules_fail_closed_and_a_streamed_frame_validates() -> TestResult {
    let ctx = noop()?;
    let mut result: Result<(), Box<dyn std::error::Error>> = Ok(());
    let errors = validation_errors(&ctx.device, || {
        result = (|| {
            let mut r = rig(&ctx, small())?;
            let q = &ctx.queue;
            let one = [(0.0, [0.2; 3])];
            let two = [(0.0, [0.2; 3]), (0.5, [0.4; 3])];
            let w = r.renderer.world_light_mut();
            // Lightmaps: larger than a page, malformed, page slots, layers.
            assert_eq!(
                w.add_lightmap(q, &solid_lightmap(9, 1, &one)),
                Err(WorldLightError::LightmapSize)
            );
            let mut bad = solid_lightmap(2, 2, &one);
            bad.layers = vec![vec![0; 3]];
            assert_eq!(w.add_lightmap(q, &bad), Err(WorldLightError::LightmapSize));
            let p0 = w.add_lightmap(q, &solid_lightmap(8, 8, &two))?;
            let p1 = w.add_lightmap(q, &solid_lightmap(4, 4, &two))?;
            assert_eq!(
                w.add_lightmap(
                    q,
                    &solid_lightmap(2, 2, &[(0.0, [0.1; 3]), (0.3, [0.1; 3]), (0.6, [0.1; 3])])
                ),
                Err(WorldLightError::LayersFull)
            );
            let p2 = w.add_lightmap(q, &solid_lightmap(2, 2, &two))?;
            assert_eq!(
                w.add_lightmap(q, &solid_lightmap(1, 1, &one)),
                Err(WorldLightError::PagesFull)
            );
            assert_eq!(w.page_rect(p1, [1.0, 1.0], [0.0, 0.0])?, [0.5, 0.5, 0.0, 0.0]);
            w.remove_lightmap(p2)?;
            assert_eq!(w.remove_lightmap(p2), Err(WorldLightError::NotResident));
            assert_eq!(w.free_layers(), 2);
            // Probes: brick size, sector size, table collisions, bricks.
            let mut big = volume(0, 0, &one);
            big.dims = [3, 2, 2];
            assert_eq!(
                w.add_probes(q, (0, 0), SECTOR, big),
                Err(WorldLightError::ProbeSize)
            );
            assert_eq!(
                w.add_probes(q, (0, 0), 0.0, volume(0, 0, &one)),
                Err(WorldLightError::SectorSize)
            );
            w.add_probes(q, (0, 0), SECTOR, volume(0, 0, &one))?;
            assert_eq!(
                w.add_probes(q, (1, 0), SECTOR * 2.0, volume(1, 0, &one)),
                Err(WorldLightError::SectorSize)
            );
            assert_eq!(
                w.add_probes(q, (4, -4), SECTOR, volume(4, -4, &one)),
                Err(WorldLightError::TableCollision)
            );
            assert_eq!(
                w.add_probes(q, (0, 0), SECTOR, volume(0, 0, &one)),
                Err(WorldLightError::TableCollision)
            );
            w.add_probes(q, (1, 0), SECTOR, volume(1, 0, &two))?;
            assert_eq!(
                w.add_probes(q, (2, 0), SECTOR, volume(2, 0, &one)),
                Err(WorldLightError::BricksFull)
            );
            assert_eq!(w.remove_probes((4, 0)), Err(WorldLightError::NotResident));
            assert_eq!((w.resident_pages(), w.resident_sectors()), (2, 2));
            assert_eq!(
                w.frame_params(0.7),
                ([1.0 / SECTOR, 4.0, 1.0, 0.0], [0.25, 0.5, 0.5, 0.7])
            );
            // A frame with lightmapped and probe-lit instances validates.
            let _ = r.renderer.spawn_lightmapped(
                r.plane,
                r.material,
                Mat4::IDENTITY,
                p0,
                [1.0, 1.0],
                [0.0, 0.0],
            )?;
            let _ = r.renderer.spawn_lightmapped(
                r.plane,
                r.material,
                Mat4::from_translation(Vec3::X),
                p1,
                [0.5, 0.5],
                [0.5, 0.0],
            )?;
            let _ = r
                .renderer
                .scene_mut()
                .spawn(r.plane, r.material, Mat4::from_translation(Vec3::Z))?;
            let stale =
                r.renderer
                    .spawn_lightmapped(r.plane, r.material, Mat4::IDENTITY, p2, [1.0; 2], [0.0; 2]);
            assert!(stale.is_err(), "a released page is refused");
            let frame = inputs(top_camera(Vec3::ZERO));
            let _ = encode_frame(&ctx, &mut r.renderer, &frame)?;
            assert_eq!(r.renderer.stats().batches, 2, "lightmapped and probe-lit batches");
            Ok(())
        })();
    });
    result?;
    assert_eq!(errors, None);
    Ok(())
}
