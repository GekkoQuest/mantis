//! Two-phase occlusion culling: hidden instances stop being drawn after one frame of
//! history, the image is identical to frustum-only culling every frame, and an instance
//! revealed by a vanished occluder is drawn in the same frame (no pop-in).

#![expect(clippy::cast_precision_loss)] // Test placement.

use glam::{Mat4, Vec3};
use mantis_formats::material::{
    ColorDefault, Deformations, LightingModel, MaterialAsset, MaterialBindings, MaterialGraph, Node, NodeId,
    SurfaceOutputs,
};
use mantis_formats::sh::ShL1;
use mantis_render::gpu::GpuContext;
use mantis_render::gpu_test::{hardware_or_skip, read_buffer, read_texture_4bpp};
use mantis_render::math::Camera;
use mantis_render::mesh::cube;
use mantis_render::post::PostSettings;
use mantis_render::renderer::{FrameInputs, Renderer, RendererConfig};
use mantis_render::scene::{DrawIndexedIndirect, InstanceHandle};

type TestResult = Result<(), Box<dyn std::error::Error>>;
type Error = Box<dyn std::error::Error>;

const SIZE: u32 = 128;

fn unlit(rgb: [f32; 3]) -> Result<MaterialAsset, Error> {
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
        outline: None,
    };
    Ok(MaterialAsset {
        graph: graph.validate()?,
        casts_shadows: false,
        deformations: Deformations::NONE,
        bindings: MaterialBindings {
            textures: Vec::new(),
            scalars: Vec::new(),
            colors: vec![ColorDefault {
                name: "color".to_owned(),
                value: [rgb[0], rgb[1], rgb[2], 1.0],
            }],
        },
    })
}

struct Scene {
    renderer: Renderer,
    wall: InstanceHandle,
}

/// A wall cube in front of a grid of 25 small cubes, and two small cubes in front of the
/// wall. Material 0 is the wall, material 1 the small cubes (batch order).
fn scene(ctx: &GpuContext, occlusion: bool) -> Result<Scene, Error> {
    let mut config = RendererConfig::new(SIZE, SIZE, wgpu::TextureFormat::Rgba8Unorm);
    config.max_instances = 64;
    config.max_vertices = 1024;
    config.max_indices = 2048;
    config.shadow_resolution = 256;
    config.occlusion = occlusion;
    let mut renderer = Renderer::new(&ctx.device, &ctx.queue, false, config)?;
    let (v, i) = cube();
    let mesh = renderer.add_mesh(&ctx.queue, &v, &i)?;
    let wall_material = renderer.add_material(&ctx.device, &ctx.queue, unlit([0.2, 0.2, 0.8])?, [None; 4])?;
    let small = renderer.add_material(&ctx.device, &ctx.queue, unlit([0.9, 0.6, 0.1])?, [None; 4])?;
    let wall = renderer.scene_mut().spawn(
        mesh,
        wall_material,
        Mat4::from_scale_rotation_translation(
            Vec3::splat(8.0),
            glam::Quat::IDENTITY,
            Vec3::new(0.0, 0.0, 5.0),
        ),
    )?;
    for gx in 0..5 {
        for gy in 0..5 {
            let p = Vec3::new(gx as f32 - 2.0, gy as f32 - 2.0, 14.0);
            let _ = renderer.scene_mut().spawn(
                mesh,
                small,
                Mat4::from_scale_rotation_translation(Vec3::splat(0.5), glam::Quat::IDENTITY, p),
            )?;
        }
    }
    for x in [-0.8f32, 0.8] {
        let _ = renderer.scene_mut().spawn(
            mesh,
            small,
            Mat4::from_scale_rotation_translation(
                Vec3::splat(0.4),
                glam::Quat::IDENTITY,
                Vec3::new(x, 0.0, -1.0),
            ),
        )?;
    }
    Ok(Scene { renderer, wall })
}

fn inputs() -> FrameInputs {
    FrameInputs {
        camera: Camera {
            position: Vec3::new(0.0, 0.0, -3.0),
            yaw: 0.0,
            pitch: 0.0,
            fov_y: 1.0,
            aspect: 1.0,
            near: 0.1,
        },
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

fn frame(ctx: &GpuContext, r: &mut Renderer) -> Result<Vec<u8>, Error> {
    let t = ctx.device.create_texture(&wgpu::TextureDescriptor {
        label: None,
        size: wgpu::Extent3d {
            width: SIZE,
            height: SIZE,
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
    let _ = r.prepare(&inputs());
    let mut encoder = ctx
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
    r.encode(&ctx.device, &ctx.queue, &mut encoder, (&t, &v))?;
    let _ = ctx.queue.submit(Some(encoder.finish()));
    Ok(read_texture_4bpp(ctx, &t)?)
}

/// Instances drawn per batch this frame: (early, late).
fn drawn(ctx: &GpuContext, r: &Renderer) -> Result<(Vec<u32>, Vec<u32>), Error> {
    let counts = |b: &wgpu::Buffer| -> Result<Vec<u32>, Error> {
        let bytes = read_buffer(ctx, b, 2 * 20)?;
        let args: &[DrawIndexedIndirect] = bytemuck::try_cast_slice(&bytes).map_err(|e| format!("{e:?}"))?;
        Ok(args.iter().map(|a| a.instance_count).collect())
    };
    Ok((
        counts(r.camera_args().ok_or("args")?)?,
        counts(r.camera_late_args())?,
    ))
}

#[test]
fn hidden_instances_are_culled_without_changing_a_pixel() -> TestResult {
    let Some(ctx) = hardware_or_skip("hidden_instances_are_culled_without_changing_a_pixel") else {
        return Ok(());
    };
    let mut occluded = scene(&ctx, true)?;
    let mut reference = scene(&ctx, false)?;
    // Frame 1: no history, everything in view goes late. Frame 2: everything drawn early,
    // the late cull learns what is hidden. Frame 3 on: hidden cubes are not drawn.
    for n in 0..4 {
        let a = frame(&ctx, &mut occluded.renderer)?;
        let b = frame(&ctx, &mut reference.renderer)?;
        assert!(a == b, "frame {n}: occlusion culling changed the image");
        let (early, late) = drawn(&ctx, &occluded.renderer)?;
        match n {
            0 => assert_eq!((early.clone(), late.clone()), (vec![0, 0], vec![1, 27])),
            1 => assert_eq!((early.clone(), late.clone()), (vec![1, 27], vec![0, 0])),
            _ => assert_eq!(
                (early.clone(), late.clone()),
                (vec![1, 2], vec![0, 0]),
                "frame {n}: the 25 hidden cubes are culled"
            ),
        }
    }
    let lit = frame(&ctx, &mut occluded.renderer)?
        .as_chunks::<4>()
        .0
        .iter()
        .filter(|[r, ..]| *r > 100)
        .count();
    assert!(lit > 0, "the front cubes are drawn");
    // The wall vanishes: the cubes behind it are drawn in the same frame.
    occluded.renderer.despawn(occluded.wall)?;
    reference.renderer.despawn(reference.wall)?;
    let a = frame(&ctx, &mut occluded.renderer)?;
    let b = frame(&ctx, &mut reference.renderer)?;
    assert!(a == b, "revealed instances appear without a frame of delay");
    let (early, late) = drawn(&ctx, &occluded.renderer)?;
    assert_eq!(early.iter().sum::<u32>() + late.iter().sum::<u32>(), 27);
    println!("MANTIS-METRIC render_occlusion_culled culled=25 of=28 frames_to_learn=2");
    Ok(())
}
