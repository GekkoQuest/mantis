//! Deformed geometry end to end: skinned meshes (near crowd tier) and vertex animation
//! (mid and far tiers). API validation runs on the no-op backend everywhere; pixel checks
//! run on a real headless adapter (skipped and counted when none exists).

use glam::{Mat4, Vec3};
use mantis_formats::material::{
    Deformations, LightingModel, MaterialAsset, MaterialGraph, Node, NodeId, SurfaceOutputs,
};
use mantis_formats::sh::ShL1;
use mantis_render::deform::{PaletteRows, VatUpload};
use mantis_render::gpu::GpuContext;
use mantis_render::gpu_test::{hardware_or_skip, noop, read_texture_4bpp, validation_errors};
use mantis_render::gpu_types::SkinVertex;
use mantis_render::math::{Camera, Sphere};
use mantis_render::mesh::cube;
use mantis_render::post::PostSettings;
use mantis_render::renderer::{FrameInputs, Renderer, RendererConfig};
use mantis_render::scene::{MaterialId, MeshId};

type TestResult = Result<(), Box<dyn std::error::Error>>;

const SIZE: u32 = 64;
const OUTPUT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;

fn unlit(outline: bool) -> Result<MaterialAsset, Box<dyn std::error::Error>> {
    let graph = MaterialGraph {
        nodes: vec![Node::ColorParam(0), Node::Swizzle(NodeId(0), [0, 1, 2, 0], 3)],
        outputs: SurfaceOutputs {
            base_color: NodeId(1),
            alpha: None,
            emissive: None,
            alpha_cutoff: None,
        },
        lighting: LightingModel::Unlit,
        rim: None,
        outline: outline.then_some(mantis_formats::material::Outline {
            width_px: 1.0,
            color: [0.0; 3],
        }),
    };
    Ok(MaterialAsset {
        graph: graph.validate()?,
        casts_shadows: true,
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

fn inputs() -> FrameInputs {
    FrameInputs {
        camera: Camera {
            position: Vec3::new(0.0, 0.0, -6.0),
            yaw: 0.0,
            pitch: 0.0,
            fov_y: 1.0,
            aspect: 1.0,
            near: 0.1,
        },
        time: 0.0,
        sun_direction: Vec3::new(0.3, -1.0, 0.4),
        sun_color: Vec3::ONE,
        shadows: false,
        sky: ShL1::ZERO,
        ambient_intensity: 1.0,
        exposure: 1.0,
        clear_color: [0.0, 0.0, 0.0, 1.0],
        ssao_strength: 0.0,
        post: PostSettings::OFF,
    }
}

struct Rig {
    renderer: Renderer,
    material: MaterialId,
    rigid: MeshId,
    skinned: MeshId,
}

fn rig(ctx: &GpuContext, bindless: bool, outline: bool) -> Result<Rig, Box<dyn std::error::Error>> {
    let mut config = RendererConfig::new(SIZE, SIZE, OUTPUT);
    config.max_instances = 64;
    config.max_batches = 16;
    config.max_vertices = 1024;
    config.max_indices = 4096;
    config.shadow_resolution = 256;
    config.palette_slots = 3 * 64;
    config.vat_slots = 4096;
    let mut renderer = Renderer::new(&ctx.device, &ctx.queue, bindless, config)?;
    let material = renderer.add_material(&ctx.device, &ctx.queue, unlit(outline)?, [None; 4])?;
    let (v, i) = cube();
    let rigid = renderer.add_mesh(&ctx.queue, &v, &i)?;
    // Two bones: the cube's +x half follows bone 1, the rest bone 0.
    let skin: Vec<SkinVertex> = v
        .iter()
        .map(|vert| {
            let joint = u16::from(vert.position[0] > 0.0);
            SkinVertex {
                joints: [joint, 0, 0, 0],
                weights: [255, 0, 0, 0],
            }
        })
        .collect();
    let skinned = renderer.add_skinned_mesh(&ctx.queue, &v, &skin, &i)?;
    Ok(Rig {
        renderer,
        material,
        rigid,
        skinned,
    })
}

fn translation_rows(t: Vec3) -> PaletteRows {
    [[1.0, 0.0, 0.0, t.x], [0.0, 1.0, 0.0, t.y], [0.0, 0.0, 1.0, t.z]]
}

/// A two-frame animation of the cube: frame 0 shifted by `a`, frame 1 by `b`.
fn cube_vat(a: Vec3, b: Vec3) -> (Vec<[f32; 4]>, Vec<[f32; 4]>, usize) {
    let (v, _) = cube();
    let mut positions = Vec::new();
    let mut normals = Vec::new();
    for shift in [a, b] {
        for vert in &v {
            let p = Vec3::from(vert.position) + shift;
            positions.push([p.x, p.y, p.z, 1.0]);
            normals.push([vert.normal[0], vert.normal[1], vert.normal[2], 0.0]);
        }
    }
    (positions, normals, v.len())
}

fn render(
    ctx: &GpuContext,
    r: &mut Renderer,
    frame: &FrameInputs,
) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let tex = ctx.device.create_texture(&wgpu::TextureDescriptor {
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
    let view = tex.create_view(&wgpu::TextureViewDescriptor::default());
    let _ = r.prepare(frame);
    let mut encoder = ctx
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("frame") });
    r.encode(&ctx.device, &ctx.queue, &mut encoder, (&tex, &view))?;
    let _ = ctx.queue.submit(Some(encoder.finish()));
    Ok(read_texture_4bpp(ctx, &tex)?)
}

/// Lit pixel count and horizontal extent `(min x, max x)`.
fn coverage(px: &[u8]) -> (usize, u32, u32) {
    let mut n = 0;
    let (mut lo, mut hi) = (u32::MAX, 0);
    for (i, p) in px.as_chunks::<4>().0.iter().enumerate() {
        if p.first().is_some_and(|r| *r > 128) {
            let x = u32::try_from(i).unwrap_or(0) % SIZE;
            n += 1;
            lo = lo.min(x);
            hi = hi.max(x);
        }
    }
    (n, lo, hi)
}

const BOUNDS: Sphere = Sphere {
    center: Vec3::ZERO,
    radius: 3.0,
};

#[test]
fn deformed_frames_pass_api_validation() -> TestResult {
    let ctx = noop()?;
    let mut result: TestResult = Ok(());
    let errors = validation_errors(&ctx.device, || {
        result = (|| {
            for bindless in [false, true] {
                let mut rig = rig(&ctx, bindless && ctx.capabilities.bindless_textures, true)?;
                let skinned =
                    rig.renderer
                        .spawn_skinned(rig.skinned, rig.material, Mat4::IDENTITY, 2, BOUNDS)?;
                rig.renderer.set_palette(
                    skinned,
                    &[translation_rows(Vec3::ZERO), translation_rows(Vec3::X)],
                )?;
                let (p, n, verts) = cube_vat(Vec3::NEG_X, Vec3::X);
                let vat = rig.renderer.add_vat(
                    &ctx.queue,
                    &VatUpload {
                        vertex_count: u32::try_from(verts)?,
                        frame_count: 2,
                        seconds_per_frame: 1.0,
                        looping: true,
                        positions: &p,
                        normals: &n,
                        bounds_min: [-1.5; 3],
                        bounds_max: [1.5; 3],
                    },
                )?;
                let crowd = rig.renderer.spawn_vat(
                    rig.rigid,
                    rig.material,
                    Mat4::from_translation(Vec3::Y),
                    vat,
                    0.25,
                )?;
                let _ = rig.renderer.scene_mut().spawn(
                    rig.rigid,
                    rig.material,
                    Mat4::from_translation(Vec3::NEG_Y),
                )?;
                let mut frame = inputs();
                frame.shadows = true;
                frame.post = PostSettings::default();
                for step in 0..3u8 {
                    rig.renderer.set_vat_time(crowd, vat, f32::from(step) * 0.4)?;
                    let _ = render(&ctx, &mut rig.renderer, &frame)?;
                }
                assert_eq!(rig.renderer.stats().instances, 3);
                rig.renderer.despawn(skinned)?;
            }
            Ok(())
        })();
    });
    result?;
    assert_eq!(errors, None);
    Ok(())
}

#[test]
fn skinning_moves_vertices_by_bone() -> TestResult {
    let Some(ctx) = hardware_or_skip("skinning_moves_vertices_by_bone") else {
        return Ok(());
    };
    let frame = inputs();
    // Reference: rigid cubes, at the origin and shifted.
    let mut reference = rig(&ctx, false, false)?;
    let h = reference
        .renderer
        .scene_mut()
        .spawn(reference.rigid, reference.material, Mat4::IDENTITY)?;
    let rest = coverage(&render(&ctx, &mut reference.renderer, &frame)?);
    reference
        .renderer
        .scene_mut()
        .set_transform(h, Mat4::from_translation(Vec3::X))?;
    let shifted = coverage(&render(&ctx, &mut reference.renderer, &frame)?);
    assert!(shifted.1 > rest.1 && shifted.2 > rest.2, "{rest:?} {shifted:?}");

    for bindless in [false, true] {
        if bindless && !ctx.capabilities.bindless_textures {
            continue;
        }
        let mut r = rig(&ctx, bindless, false)?;
        let s = r
            .renderer
            .spawn_skinned(r.skinned, r.material, Mat4::IDENTITY, 2, BOUNDS)?;
        // Identity palette: the bind pose.
        assert_eq!(
            coverage(&render(&ctx, &mut r.renderer, &frame)?),
            rest,
            "bind pose"
        );
        // Both bones shifted: the whole cube moves like the rigid reference.
        r.renderer
            .set_palette(s, &[translation_rows(Vec3::X), translation_rows(Vec3::X)])?;
        assert_eq!(
            coverage(&render(&ctx, &mut r.renderer, &frame)?),
            shifted,
            "both bones"
        );
        // Only the +x half's bone shifted: the cube stretches to the right.
        r.renderer
            .set_palette(s, &[translation_rows(Vec3::ZERO), translation_rows(Vec3::X)])?;
        let stretched = coverage(&render(&ctx, &mut r.renderer, &frame)?);
        assert_eq!(
            (stretched.1, stretched.2),
            (rest.1, shifted.2),
            "stretched {stretched:?}"
        );
        assert!(stretched.0 > rest.0);
    }
    Ok(())
}

#[test]
fn vertex_animation_blends_between_frames() -> TestResult {
    let Some(ctx) = hardware_or_skip("vertex_animation_blends_between_frames") else {
        return Ok(());
    };
    let frame = inputs();
    let mut reference = rig(&ctx, false, false)?;
    let h = reference
        .renderer
        .scene_mut()
        .spawn(reference.rigid, reference.material, Mat4::IDENTITY)?;
    let mut at = |x: f32| -> Result<(usize, u32, u32), Box<dyn std::error::Error>> {
        reference
            .renderer
            .scene_mut()
            .set_transform(h, Mat4::from_translation(Vec3::X * x))?;
        Ok(coverage(&render(&ctx, &mut reference.renderer, &frame)?))
    };
    let (left, centre, right) = (at(-1.0)?, at(0.0)?, at(1.0)?);

    let mut r = rig(&ctx, false, false)?;
    let (positions, normals, verts) = cube_vat(Vec3::NEG_X, Vec3::X);
    let vat = r.renderer.add_vat(
        &ctx.queue,
        &VatUpload {
            vertex_count: u32::try_from(verts)?,
            frame_count: 2,
            seconds_per_frame: 1.0,
            looping: false,
            positions: &positions,
            normals: &normals,
            bounds_min: [-1.5, -0.5, -0.5],
            bounds_max: [1.5, 0.5, 0.5],
        },
    )?;
    let animated = r
        .renderer
        .spawn_vat(r.rigid, r.material, Mat4::IDENTITY, vat, 0.0)?;
    assert_eq!(coverage(&render(&ctx, &mut r.renderer, &frame)?), left, "frame 0");
    r.renderer.set_vat_time(animated, vat, 0.5)?;
    assert_eq!(
        coverage(&render(&ctx, &mut r.renderer, &frame)?),
        centre,
        "halfway"
    );
    r.renderer.set_vat_time(animated, vat, 7.0)?;
    assert_eq!(
        coverage(&render(&ctx, &mut r.renderer, &frame)?),
        right,
        "clamped on the last frame"
    );
    Ok(())
}

#[test]
fn palettes_fill_and_free_their_region() -> TestResult {
    let ctx = noop()?;
    let mut r = rig(&ctx, false, false)?;
    let mut frame = inputs();
    frame.post = PostSettings::default();
    // The palette region holds 64 bones: two 32-bone instances fill it.
    let a = r
        .renderer
        .spawn_skinned(r.skinned, r.material, Mat4::IDENTITY, 32, BOUNDS)?;
    let b = r
        .renderer
        .spawn_skinned(r.skinned, r.material, Mat4::IDENTITY, 32, BOUNDS)?;
    assert!(
        r.renderer
            .spawn_skinned(r.skinned, r.material, Mat4::IDENTITY, 1, BOUNDS)
            .is_err(),
        "full"
    );
    r.renderer.set_palette(a, &[translation_rows(Vec3::Y)])?;
    let _ = r.renderer.prepare(&frame);
    r.renderer.despawn(b)?;
    let c = r
        .renderer
        .spawn_skinned(r.skinned, r.material, Mat4::IDENTITY, 32, BOUNDS)?;
    assert!(
        r.renderer
            .set_palette(c, &[translation_rows(Vec3::ZERO); 33])
            .is_err(),
        "more entries than bones"
    );
    assert!(r.renderer.set_palette(b, &[]).is_err(), "stale handle");
    Ok(())
}
