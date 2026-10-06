//! The forward renderer end to end. API validation runs on the no-op backend everywhere;
//! pixel checks run on a real headless adapter (skipped and counted when none exists).

#![expect(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)] // Pixel math in tests.

use glam::{Mat4, Vec3};
use mantis_formats::color_grading::ColorGrading;
use mantis_formats::material::{
    Deformations, LightingModel, MaterialAsset, MaterialGraph, Node, NodeId, Outline, SurfaceOutputs,
    reference_materials,
};
use mantis_formats::sh::ShL1;
use mantis_render::gpu::GpuContext;
use mantis_render::gpu_test::{hardware_or_skip, noop, read_buffer, read_texture_4bpp, validation_errors};
use mantis_render::lighting::clusters::PointLight;
use mantis_render::materials::MaterialLoadError;
use mantis_render::math::Camera;
use mantis_render::mesh::{cube, plane};
use mantis_render::post::PostSettings;
use mantis_render::renderer::{FrameInputs, Renderer, RendererConfig, RendererError};
use mantis_render::scene::{DrawIndexedIndirect, MeshId};

type TestResult = Result<(), Box<dyn std::error::Error>>;

const SIZE: u32 = 64;
const OUTPUT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;

/// A flat-colored material: color parameter 0, with the given lighting.
fn flat(
    lighting: LightingModel,
    outline: Option<Outline>,
    casts_shadows: bool,
) -> Result<MaterialAsset, Box<dyn std::error::Error>> {
    let graph = MaterialGraph {
        nodes: vec![Node::ColorParam(0), Node::Swizzle(NodeId(0), [0, 1, 2, 0], 3)],
        outputs: SurfaceOutputs {
            base_color: NodeId(1),
            alpha: None,
            emissive: None,
            alpha_cutoff: None,
        },
        lighting,
        rim: None,
        outline,
    };
    Ok(MaterialAsset {
        graph: graph.validate()?,
        casts_shadows,
        deformations: Deformations::ALL,
        bindings: mantis_formats::material::MaterialBindings::default(),
    })
}

/// `m` with color parameter 0 defaulting to `rgb`.
fn colored(mut m: MaterialAsset, rgb: [f32; 3]) -> MaterialAsset {
    m.bindings.colors = vec![mantis_formats::material::ColorDefault {
        name: "color".to_owned(),
        value: [rgb[0], rgb[1], rgb[2], 1.0],
    }];
    m
}

fn inputs(camera: Camera) -> FrameInputs {
    FrameInputs {
        camera,
        time: 0.0,
        sun_direction: Vec3::new(0.0, 0.0, 1.0),
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

fn front_camera() -> Camera {
    Camera {
        position: Vec3::new(0.0, 0.0, -3.0),
        yaw: 0.0,
        pitch: 0.0,
        fov_y: 1.0,
        aspect: 1.0,
        near: 0.1,
    }
}

struct Rig {
    renderer: Renderer,
    cube: MeshId,
    plane: MeshId,
}

fn new_rig(ctx: &GpuContext) -> Result<Rig, Box<dyn std::error::Error>> {
    rig_with(ctx, ctx.capabilities.bindless_textures)
}

fn rig_with(ctx: &GpuContext, bindless: bool) -> Result<Rig, Box<dyn std::error::Error>> {
    let mut config = RendererConfig::new(SIZE, SIZE, OUTPUT);
    config.max_instances = 256;
    config.max_batches = 32;
    config.max_vertices = 4096;
    config.max_indices = 8192;
    config.shadow_resolution = 512;
    let mut renderer = Renderer::new(&ctx.device, &ctx.queue, bindless, config)?;
    let (cv, ci) = cube();
    let cube = renderer.add_mesh(&ctx.queue, &cv, &ci)?;
    let (pv, pi) = plane(20.0);
    let plane = renderer.add_mesh(&ctx.queue, &pv, &pi)?;
    Ok(Rig {
        renderer,
        cube,
        plane,
    })
}

fn output_texture(ctx: &GpuContext) -> (wgpu::Texture, wgpu::TextureView) {
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
    (t, v)
}

/// Renders one frame and returns the RGBA8 pixels.
fn render(
    ctx: &GpuContext,
    r: &mut Renderer,
    frame: &FrameInputs,
) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let (tex, view) = output_texture(ctx);
    let _ = r.prepare(frame);
    let mut encoder = ctx
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("frame") });
    r.encode(&ctx.device, &ctx.queue, &mut encoder, (&tex, &view))?;
    let _ = ctx.queue.submit(Some(encoder.finish()));
    Ok(read_texture_4bpp(ctx, &tex)?)
}

fn pixel(pixels: &[u8], x: u32, y: u32) -> [u8; 4] {
    let i = ((y * SIZE + x) * 4) as usize;
    let mut out = [0u8; 4];
    if let Some(s) = pixels.get(i..i + 4) {
        out.copy_from_slice(s);
    }
    out
}

/// The renderer's ACES curve on the CPU, to 8 bits.
fn aces8(x: f32) -> u8 {
    let y = (x * (2.51 * x + 0.03)) / (x * (2.43 * x + 0.59) + 0.14);
    (y.clamp(0.0, 1.0) * 255.0).round() as u8
}

fn close(a: u8, b: u8) -> bool {
    a.abs_diff(b) <= 2
}

#[test]
fn a_full_frame_passes_api_validation() -> TestResult {
    let ctx = noop()?;
    let mut result: Result<(), Box<dyn std::error::Error>> = Ok(());
    let errors = validation_errors(&ctx.device, || {
        result = (|| {
            for bindless in [false, true] {
                let mut rig = rig_with(&ctx, bindless && ctx.capabilities.bindless_textures)?;
                for (i, m) in reference_materials()?.into_iter().enumerate() {
                    let id = rig.renderer.add_material(&ctx.device, &ctx.queue, m, [None; 4])?;
                    let mesh = if i % 2 == 0 { rig.cube } else { rig.plane };
                    let _ = rig.renderer.scene_mut().spawn(
                        mesh,
                        id,
                        Mat4::from_translation(Vec3::new(i as f32, 0.0, 0.0)),
                    )?;
                }
                rig.renderer.set_lights(&[PointLight {
                    position: Vec3::Y,
                    range: 5.0,
                    color: Vec3::ONE,
                    intensity: 2.0,
                }]);
                let mut frame = inputs(front_camera());
                frame.shadows = true;
                frame.ssao_strength = 1.0;
                frame.post = PostSettings::default();
                let stats = rig.renderer.prepare(&frame);
                assert_eq!((stats.instances, stats.lights, stats.cascades), (4, 1, 4));
                let (tex, view) = output_texture(&ctx);
                let mut encoder = ctx
                    .device
                    .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
                rig.renderer
                    .encode(&ctx.device, &ctx.queue, &mut encoder, (&tex, &view))?;
                let _ = ctx.queue.submit(Some(encoder.finish()));
            }
            Ok(())
        })();
    });
    result?;
    assert_eq!(errors, None);
    Ok(())
}

#[test]
fn lambert_and_toon_responses_match_the_cpu_formula() -> TestResult {
    let Some(ctx) = hardware_or_skip("lambert_and_toon_responses_match_the_cpu_formula") else {
        return Ok(());
    };
    // Sun at n . l = 0.6 on the cube's front face; no ambient, no AO, no shadows.
    let toon = LightingModel::Toon {
        bands: 3,
        softness: 0.0,
        shadow_tint: [0.0; 3],
    };
    // Both texture modes when the device has bindless support: the results must agree.
    let modes: Vec<bool> = if ctx.capabilities.bindless_textures {
        vec![false, true]
    } else {
        vec![false]
    };
    for (bindless, (lighting, level)) in modes
        .iter()
        .flat_map(|b| [(*b, (LightingModel::Lambert, 0.6f32)), (*b, (toon, 2.0 / 3.0))])
    {
        let mut rig = rig_with(&ctx, bindless)?;
        let m = rig.renderer.add_material(
            &ctx.device,
            &ctx.queue,
            colored(flat(lighting, None, false)?, [0.5; 3]),
            [None; 4],
        )?;
        let _ = rig.renderer.scene_mut().spawn(rig.cube, m, Mat4::IDENTITY)?;
        let mut frame = inputs(front_camera());
        frame.sun_direction = Vec3::new(0.8, 0.0, 0.6);
        let px = render(&ctx, &mut rig.renderer, &frame)?;
        let center = pixel(&px, SIZE / 2, SIZE / 2);
        let expected = aces8(0.5 * level);
        assert!(
            close(center[0], expected),
            "{lighting:?}: {center:?}, expected {expected}"
        );
        assert_eq!(pixel(&px, 1, 1), [0, 0, 0, 255], "background");
    }
    Ok(())
}

#[test]
fn objects_cast_shadows_on_the_ground() -> TestResult {
    let Some(ctx) = hardware_or_skip("objects_cast_shadows_on_the_ground") else {
        return Ok(());
    };
    let mut rig = new_rig(&ctx)?;
    let m = rig.renderer.add_material(
        &ctx.device,
        &ctx.queue,
        colored(flat(LightingModel::Lambert, None, true)?, [0.8; 3]),
        [None; 4],
    )?;
    let _ = rig.renderer.scene_mut().spawn(rig.plane, m, Mat4::IDENTITY)?;
    // A caster floating above the ground, offset left of center.
    let _ = rig
        .renderer
        .scene_mut()
        .spawn(rig.cube, m, Mat4::from_translation(Vec3::new(-1.5, 1.5, 0.0)))?;
    let camera = Camera {
        position: Vec3::new(0.0, 8.0, -0.01),
        yaw: 0.0,
        pitch: -core::f32::consts::FRAC_PI_2 + 0.01,
        fov_y: 1.0,
        aspect: 1.0,
        near: 0.1,
    };
    let mut frame = inputs(camera);
    frame.shadows = true;
    // The camera looks straight down; the sun travels +x and down, so the cube's shadow
    // falls 1.5 units to +x of it, onto the ground at the origin, which the camera sees.
    frame.sun_direction = Vec3::new(1.0, -1.0, 0.0);
    let px = render(&ctx, &mut rig.renderer, &frame)?;
    // With the sun travelling +x and down, the shadow lands 1.5 units to +x of the cube.
    let shadow_clip = camera.view_projection() * glam::Vec4::new(0.0, 0.0, 0.0, 1.0);
    let sn = shadow_clip.truncate() / shadow_clip.w;
    let (hx, hy) = (
        ((sn.x * 0.5 + 0.5) * SIZE as f32) as u32,
        ((0.5 - sn.y * 0.5) * SIZE as f32) as u32,
    );
    let open_clip = camera.view_projection() * glam::Vec4::new(2.5, 0.0, 2.5, 1.0);
    let on = open_clip.truncate() / open_clip.w;
    let (ox, oy) = (
        ((on.x * 0.5 + 0.5) * SIZE as f32) as u32,
        ((0.5 - on.y * 0.5) * SIZE as f32) as u32,
    );
    let shadowed = pixel(&px, hx, hy)[0];
    let open = pixel(&px, ox, oy)[0];
    assert!(
        open > 40 && shadowed + 30 < open,
        "shadowed {shadowed}, open {open}"
    );
    Ok(())
}

#[test]
fn clustered_point_lights_light_within_range_only() -> TestResult {
    let Some(ctx) = hardware_or_skip("clustered_point_lights_light_within_range_only") else {
        return Ok(());
    };
    let mut rig = new_rig(&ctx)?;
    let m = rig.renderer.add_material(
        &ctx.device,
        &ctx.queue,
        colored(flat(LightingModel::Lambert, None, false)?, [1.0; 3]),
        [None; 4],
    )?;
    let _ = rig.renderer.scene_mut().spawn(rig.plane, m, Mat4::IDENTITY)?;
    let camera = Camera {
        position: Vec3::new(0.0, 8.0, -0.01),
        yaw: 0.0,
        pitch: -core::f32::consts::FRAC_PI_2 + 0.01,
        fov_y: 1.2,
        aspect: 1.0,
        near: 0.1,
    };
    let mut frame = inputs(camera);
    frame.sun_color = Vec3::ZERO;
    rig.renderer.set_lights(&[PointLight {
        position: Vec3::new(0.0, 0.5, 0.0),
        range: 2.0,
        color: Vec3::new(1.0, 0.0, 0.0),
        intensity: 4.0,
    }]);
    let px = render(&ctx, &mut rig.renderer, &frame)?;
    let center = pixel(&px, SIZE / 2, SIZE / 2);
    let edge = pixel(&px, 2, SIZE / 2);
    assert!(center[0] > 100, "lit by the red light: {center:?}");
    assert_eq!((center[1], center[2]), (0, 0), "red light only");
    assert_eq!(edge[0], 0, "outside the light's range: {edge:?}");
    Ok(())
}

#[test]
fn outlines_and_alpha_test_render_as_specified() -> TestResult {
    let Some(ctx) = hardware_or_skip("outlines_and_alpha_test_render_as_specified") else {
        return Ok(());
    };
    let mut rig = new_rig(&ctx)?;
    let outline = Outline {
        width_px: 3.0,
        color: [1.0, 0.0, 1.0],
    };
    let m = rig.renderer.add_material(
        &ctx.device,
        &ctx.queue,
        colored(flat(LightingModel::Unlit, Some(outline), false)?, [0.0, 1.0, 0.0]),
        [None; 4],
    )?;
    let _ = rig.renderer.scene_mut().spawn(rig.cube, m, Mat4::IDENTITY)?;
    let px = render(&ctx, &mut rig.renderer, &inputs(front_camera()))?;
    // Scan the middle row: background, outline (magenta), cube (green), outline, background.
    let row: Vec<[u8; 4]> = (0..SIZE).map(|x| pixel(&px, x, SIZE / 2)).collect();
    let magenta = row
        .iter()
        .filter(|p| p[0] > 150 && p[1] < 30 && p[2] > 150)
        .count();
    let green = row.iter().filter(|p| p[1] > 150 && p[0] < 30).count();
    assert!(green > 10, "cube body");
    assert!(
        (4..=10).contains(&magenta),
        "two outline rims about 3 px each: {magenta}"
    );
    // Alpha test: the reference foliage material samples a white texture (alpha 1) by
    // default, so it renders; an explicitly transparent slot discards everything.
    let foliage = reference_materials()?.into_iter().nth(1).ok_or("foliage")?;
    let transparent = mantis_render::textures::solid_2d(&ctx.device, &ctx.queue, "clear", [255, 255, 255, 0]);
    let mut rig2 = new_rig(&ctx)?;
    let f = rig2.renderer.add_material(
        &ctx.device,
        &ctx.queue,
        colored(foliage, [1.0; 3]),
        [None, Some(&transparent), None, None],
    )?;
    let _ = rig2.renderer.scene_mut().spawn(rig2.cube, f, Mat4::IDENTITY)?;
    let px = render(&ctx, &mut rig2.renderer, &inputs(front_camera()))?;
    assert_eq!(
        pixel(&px, SIZE / 2, SIZE / 2),
        [0, 0, 0, 255],
        "fully transparent foliage is discarded"
    );
    Ok(())
}

#[test]
fn gpu_culling_drops_objects_outside_the_view() -> TestResult {
    let Some(ctx) = hardware_or_skip("gpu_culling_drops_objects_outside_the_view") else {
        return Ok(());
    };
    let mut rig = new_rig(&ctx)?;
    let a = rig.renderer.add_material(
        &ctx.device,
        &ctx.queue,
        colored(flat(LightingModel::Lambert, None, false)?, [1.0; 3]),
        [None; 4],
    )?;
    let b = rig.renderer.add_material(
        &ctx.device,
        &ctx.queue,
        colored(flat(LightingModel::Unlit, None, false)?, [1.0; 3]),
        [None; 4],
    )?;
    for x in 0..3 {
        let _ = rig.renderer.scene_mut().spawn(
            rig.cube,
            a,
            Mat4::from_translation(Vec3::new(x as f32 - 1.0, 0.0, 0.0)),
        )?;
    }
    let _ =
        rig.renderer
            .scene_mut()
            .spawn(rig.cube, b, Mat4::from_translation(Vec3::new(0.0, 0.0, -20.0)))?; // behind the camera
    let _ = render(&ctx, &mut rig.renderer, &inputs(front_camera()))?;
    // The first frame has no visibility history: everything visible is in the late list.
    let counts = |buffer: &wgpu::Buffer| -> Result<Vec<u32>, Box<dyn std::error::Error>> {
        let bytes = read_buffer(&ctx, buffer, 2 * 20)?;
        let args: &[DrawIndexedIndirect] = bytemuck::try_cast_slice(&bytes).map_err(|e| format!("{e:?}"))?;
        Ok(args.iter().map(|x| x.instance_count).collect())
    };
    let early = counts(rig.renderer.camera_args().ok_or("args")?)?;
    let late = counts(rig.renderer.camera_late_args())?;
    let total: Vec<u32> = early.iter().zip(&late).map(|(a, b)| a + b).collect();
    assert_eq!(total, vec![3, 0], "material order: a (three visible), b (culled)");
    assert_eq!(early, vec![0, 0], "nothing was visible before the first frame");
    Ok(())
}

/// An unlit cube of `rgb` in front of the camera, rotated `roll` radians about the view
/// axis.
fn unlit_cube(ctx: &GpuContext, rgb: [f32; 3], roll: f32) -> Result<Rig, Box<dyn std::error::Error>> {
    let mut rig = new_rig(ctx)?;
    let m = rig.renderer.add_material(
        &ctx.device,
        &ctx.queue,
        colored(flat(LightingModel::Unlit, None, false)?, rgb),
        [None; 4],
    )?;
    let _ = rig
        .renderer
        .scene_mut()
        .spawn(rig.cube, m, Mat4::from_rotation_z(roll))?;
    Ok(rig)
}

/// Pixels strictly between the background and the flat interior value (anti-aliased edges).
fn partial_pixels(px: &[u8], interior: u8) -> usize {
    px.chunks(4)
        .filter(|p| {
            p.first()
                .is_some_and(|r| *r > 12 && r.saturating_add(12) < interior)
        })
        .count()
}

#[test]
fn grading_and_bloom_apply_from_data() -> TestResult {
    let Some(ctx) = hardware_or_skip("grading_and_bloom_apply_from_data") else {
        return Ok(());
    };
    // Grading: saturation 0 turns a colored surface gray.
    let mut rig = unlit_cube(&ctx, [0.9, 0.3, 0.1], 0.0)?;
    let frame = inputs(front_camera());
    let colored = pixel(&render(&ctx, &mut rig.renderer, &frame)?, SIZE / 2, SIZE / 2);
    assert!(colored[0] > colored[1].saturating_add(40), "{colored:?}");
    rig.renderer.set_grading(
        &ctx.queue,
        &ColorGrading {
            saturation: 0.0,
            ..ColorGrading::NEUTRAL
        },
    );
    let gray = pixel(&render(&ctx, &mut rig.renderer, &frame)?, SIZE / 2, SIZE / 2);
    assert!(
        gray[0].abs_diff(gray[1]) <= 2 && gray[1].abs_diff(gray[2]) <= 2,
        "{gray:?}"
    );
    // Bloom: a bright surface glows past its silhouette only when bloom is on.
    let mut rig = unlit_cube(&ctx, [6.0; 3], 0.0)?;
    let mut frame = inputs(front_camera());
    let plain = render(&ctx, &mut rig.renderer, &frame)?;
    let top = (0..SIZE)
        .find(|y| pixel(&plain, SIZE / 2, *y)[0] > 0)
        .ok_or("cube not visible")?;
    assert!(top > 4, "cube top edge at {top}");
    let probe = top - 3;
    assert_eq!(pixel(&plain, SIZE / 2, probe)[0], 0, "no glow without bloom");
    frame.post = PostSettings {
        bloom_intensity: 0.5,
        bloom_threshold: 1.0,
        ..PostSettings::OFF
    };
    let glowing = render(&ctx, &mut rig.renderer, &frame)?;
    let glow = pixel(&glowing, SIZE / 2, probe)[0];
    assert!(glow > 8, "glow {glow} three pixels outside the silhouette");
    // A 64-pixel target is small next to the pyramid, so the glow reaches the corners,
    // but it falls off with distance.
    let corner = pixel(&glowing, 1, 1)[0];
    assert!(corner < glow, "corner {corner}, near the silhouette {glow}");
    Ok(())
}

#[test]
fn taa_converges_to_anti_aliased_edges_and_resets() -> TestResult {
    let Some(ctx) = hardware_or_skip("taa_converges_to_anti_aliased_edges_and_resets") else {
        return Ok(());
    };
    // Diagonal edges: without TAA every pixel is either background or interior.
    let mut rig = unlit_cube(&ctx, [0.5; 3], 0.4)?;
    let mut frame = inputs(front_camera());
    let aliased = render(&ctx, &mut rig.renderer, &frame)?;
    let interior = pixel(&aliased, SIZE / 2, SIZE / 2)[0];
    assert!(interior > 100, "interior {interior}");
    let hard = partial_pixels(&aliased, interior);
    frame.post = PostSettings {
        taa: true,
        taa_current_weight: 0.1,
        ..PostSettings::OFF
    };
    let mut converged = Vec::new();
    for _ in 0..24 {
        converged = render(&ctx, &mut rig.renderer, &frame)?;
    }
    let soft = partial_pixels(&converged, interior);
    let center = pixel(&converged, SIZE / 2, SIZE / 2)[0];
    assert!(center.abs_diff(interior) <= 3, "interior {center} vs {interior}");
    assert!(
        soft > hard + 20,
        "partial edge pixels: {soft} with TAA, {hard} without"
    );
    // A reset drops the history: the next frame is a single jittered sample again.
    rig.renderer.reset_history();
    let fresh = partial_pixels(&render(&ctx, &mut rig.renderer, &frame)?, interior);
    assert!(fresh + 20 < soft, "after reset {fresh}, converged {soft}");
    Ok(())
}

#[test]
fn the_velocity_target_holds_per_object_motion() -> TestResult {
    let Some(ctx) = hardware_or_skip("the_velocity_target_holds_per_object_motion") else {
        return Ok(());
    };
    let mut rig = new_rig(&ctx)?;
    let m = rig.renderer.add_material(
        &ctx.device,
        &ctx.queue,
        colored(flat(LightingModel::Unlit, None, false)?, [0.5; 3]),
        [None; 4],
    )?;
    let moving =
        rig.renderer
            .scene_mut()
            .spawn(rig.cube, m, Mat4::from_translation(Vec3::new(-0.6, 0.0, 0.0)))?;
    let still =
        rig.renderer
            .scene_mut()
            .spawn(rig.cube, m, Mat4::from_translation(Vec3::new(2.0, 0.0, 6.0)))?;
    let camera = front_camera();
    let mut frame = inputs(camera);
    frame.post = PostSettings::default();
    let _ = render(&ctx, &mut rig.renderer, &frame)?;
    // Move one cube 0.2 m right; the other stays.
    rig.renderer
        .scene_mut()
        .set_transform(moving, Mat4::from_translation(Vec3::new(-0.4, 0.0, 0.0)))?;
    let _ = rig
        .renderer
        .scene_mut()
        .set_transform(still, Mat4::from_translation(Vec3::new(2.0, 0.0, 6.0)));
    let _ = render(&ctx, &mut rig.renderer, &frame)?;
    let velocity = read_texture_rg16f(&ctx, rig.renderer.velocity_texture())?;
    let at = |p: Vec3| {
        let (x, y) = project(&camera, p);
        velocity
            .get((y * SIZE + x) as usize)
            .copied()
            .unwrap_or([f32::NAN; 2])
    };
    // The moving cube's front face center moved by the projected 0.2 m.
    let front = Vec3::new(-0.4, 0.0, -0.5);
    let before = camera.view_projection() * Vec3::new(-0.6, 0.0, -0.5).extend(1.0);
    let after = camera.view_projection() * front.extend(1.0);
    let expected_u = (after.x / after.w - before.x / before.w) * 0.5;
    let [u, v] = at(front);
    assert!(
        (u - expected_u).abs() < 2e-3 && v.abs() < 1e-3,
        "moving: {u}, {v} vs {expected_u}"
    );
    let [u, v] = at(Vec3::new(2.0, 0.0, 5.5));
    assert!(u.abs() < 1e-4 && v.abs() < 1e-4, "still: {u}, {v}");
    let [u, _] = velocity.first().copied().unwrap_or([0.0; 2]);
    assert!(u > 50.0, "nothing drawn at the corner: {u}");
    Ok(())
}

/// TAA is no longer refused for moving geometry: a full frame with motion, skinned
/// animation, and live particles validates with TAA on.
#[test]
fn taa_runs_with_moving_geometry() -> TestResult {
    let ctx = noop()?;
    let mut result: Result<(), Box<dyn std::error::Error>> = Ok(());
    let errors = validation_errors(&ctx.device, || {
        result = (|| {
            let mut rig = new_rig(&ctx)?;
            let m = rig.renderer.add_material(
                &ctx.device,
                &ctx.queue,
                colored(flat(LightingModel::Lambert, None, false)?, [0.5; 3]),
                [None; 4],
            )?;
            let h = rig.renderer.scene_mut().spawn(rig.cube, m, Mat4::IDENTITY)?;
            let mut frame = inputs(front_camera());
            frame.post = PostSettings::default();
            for i in 0..4u8 {
                rig.renderer
                    .scene_mut()
                    .set_transform(h, Mat4::from_translation(Vec3::X * f32::from(i) * 0.1))?;
                let _ = render(&ctx, &mut rig.renderer, &frame)?;
            }
            Ok(())
        })();
    });
    result?;
    assert_eq!(errors, None);
    Ok(())
}

/// An `Rg16Float` texture's texels.
fn read_texture_rg16f(
    ctx: &GpuContext,
    t: &wgpu::Texture,
) -> Result<Vec<[f32; 2]>, Box<dyn std::error::Error>> {
    let bytes = read_texture_4bpp(ctx, t)?;
    Ok(bytes
        .as_chunks::<4>()
        .0
        .iter()
        .map(|[a, b, c, d]| {
            [
                mantis_formats::half::f16_to_f32(u16::from_le_bytes([*a, *b])),
                mantis_formats::half::f16_to_f32(u16::from_le_bytes([*c, *d])),
            ]
        })
        .collect())
}

fn glow_effect() -> mantis_formats::particle_effect::ParticleEffect {
    use mantis_formats::particle_effect::{
        BlendMode, Burst, ColorKey, EmitterDef, EmitterShape, ParticleEffect, SimulationSpace, SizeKey,
    };
    ParticleEffect {
        emitters: vec![EmitterDef {
            capacity: 1,
            duration: 1.0,
            looping: false,
            rate: 0.0,
            bursts: vec![Burst { time: 0.0, count: 1 }],
            lifetime_min: 10.0,
            lifetime_max: 10.0,
            shape: EmitterShape::Point,
            speed_min: 0.0,
            speed_max: 0.0,
            acceleration: [0.0; 3],
            drag: 0.0,
            color: vec![ColorKey {
                t: 0.0,
                value: [1.0, 1.0, 1.0, 1.0],
            }],
            size: vec![SizeKey { t: 0.0, value: [1.0] }],
            blend: BlendMode::Additive,
            space: SimulationSpace::World,
        }],
    }
}

#[test]
fn particles_render_in_the_frame_graph_behind_scene_depth() -> TestResult {
    let Some(ctx) = hardware_or_skip("particles_render_in_the_frame_graph_behind_scene_depth") else {
        return Ok(());
    };
    let mut rig = new_rig(&ctx)?;
    let glow = rig
        .renderer
        .particles_mut()
        .register(&ctx.queue, &glow_effect())?;
    let at = mantis_render::particles::EmitterTransform::at(Vec3::ZERO);
    let h = rig.renderer.particles_mut().spawn(glow, at, 1)?;
    let mut frame = inputs(front_camera());
    let mut px = Vec::new();
    for step in 0..3u8 {
        frame.time = f32::from(step) / 60.0;
        px = render(&ctx, &mut rig.renderer, &frame)?;
    }
    assert_eq!(rig.renderer.stats().particle_emitters, 1);
    let lit = pixel(&px, SIZE / 2, SIZE / 2)[0];
    assert!(lit > 100, "particle centre {lit}");
    assert_eq!(pixel(&px, 1, 1)[0], 0, "background");
    // A dark cube between the camera and the particle hides it.
    let m = rig.renderer.add_material(
        &ctx.device,
        &ctx.queue,
        colored(flat(LightingModel::Unlit, None, false)?, [0.0; 3]),
        [None; 4],
    )?;
    let _ = rig
        .renderer
        .scene_mut()
        .spawn(rig.cube, m, Mat4::from_translation(Vec3::new(0.0, 0.0, -1.5)))?;
    frame.time = 4.0 / 60.0;
    let hidden = pixel(&render(&ctx, &mut rig.renderer, &frame)?, SIZE / 2, SIZE / 2)[0];
    assert!(hidden < 10, "occluded particle {hidden}");
    rig.renderer.particles_mut().kill(h)?;
    Ok(())
}

/// The pixel a world point lands on.
fn project(camera: &Camera, p: Vec3) -> (u32, u32) {
    let clip = camera.view_projection() * p.extend(1.0);
    let ndc = clip.truncate() / clip.w;
    (
        ((ndc.x * 0.5 + 0.5) * SIZE as f32) as u32,
        ((0.5 - ndc.y * 0.5) * SIZE as f32) as u32,
    )
}

#[test]
fn ssao_darkens_contact_creases_only() -> TestResult {
    let Some(ctx) = hardware_or_skip("ssao_darkens_contact_creases_only") else {
        return Ok(());
    };
    let mut rig = new_rig(&ctx)?;
    let m = rig.renderer.add_material(
        &ctx.device,
        &ctx.queue,
        colored(flat(LightingModel::Lambert, None, false)?, [0.8; 3]),
        [None; 4],
    )?;
    let _ = rig.renderer.scene_mut().spawn(rig.plane, m, Mat4::IDENTITY)?;
    // A unit cube resting on the ground.
    let _ = rig
        .renderer
        .scene_mut()
        .spawn(rig.cube, m, Mat4::from_translation(Vec3::new(0.0, 0.5, 0.0)))?;
    let camera = Camera {
        position: Vec3::new(0.0, 2.5, -3.0),
        yaw: 0.0,
        pitch: -0.6,
        fov_y: 1.2,
        aspect: 1.0,
        near: 0.1,
    };
    // Ambient light only (occlusion scales the ambient term).
    let mut frame = inputs(camera);
    frame.sun_color = Vec3::ZERO;
    frame.sky = ShL1 {
        rgb: [[1.0, 0.0, 0.0, 0.0]; 3],
    };
    let crease = project(&camera, Vec3::new(0.0, 0.0, -0.55));
    let open = project(&camera, Vec3::new(1.8, 0.0, -1.2));
    let plain = render(&ctx, &mut rig.renderer, &frame)?;
    frame.ssao_strength = 1.0;
    let occluded = render(&ctx, &mut rig.renderer, &frame)?;
    let at = |px: &[u8], p: (u32, u32)| pixel(px, p.0, p.1)[0];
    assert!(
        close(at(&plain, crease), at(&plain, open)),
        "flat ground without SSAO"
    );
    assert!(
        at(&occluded, crease) + 10 < at(&plain, crease),
        "the crease darkens: {} vs {}",
        at(&occluded, crease),
        at(&plain, crease)
    );
    assert!(
        at(&occluded, open) + 4 >= at(&plain, open),
        "open ground stays lit: {} vs {}",
        at(&occluded, open),
        at(&plain, open)
    );
    Ok(())
}

#[test]
fn material_parameters_change_by_name_without_a_rebuild() -> TestResult {
    let Some(ctx) = hardware_or_skip("material_parameters_change_by_name_without_a_rebuild") else {
        return Ok(());
    };
    let mut rig = unlit_cube(&ctx, [0.9, 0.1, 0.1], 0.0)?;
    let m = mantis_render::scene::MaterialId(0);
    let frame = inputs(front_camera());
    let red = pixel(&render(&ctx, &mut rig.renderer, &frame)?, SIZE / 2, SIZE / 2);
    assert!(
        red[0] > red[2].saturating_add(80),
        "starts at the default: {red:?}"
    );
    let loaded = rig.renderer.materials().len();
    rig.renderer
        .set_material_param(&ctx.queue, m, "color", [0.1, 0.1, 0.9, 1.0])?;
    let blue = pixel(&render(&ctx, &mut rig.renderer, &frame)?, SIZE / 2, SIZE / 2);
    assert!(blue[2] > blue[0].saturating_add(80), "recolored: {blue:?}");
    assert_eq!(rig.renderer.materials().len(), loaded, "nothing reloaded");
    assert!(
        rig.renderer
            .set_material_param(&ctx.queue, m, "missing", [0.0; 4])
            .is_err()
    );
    Ok(())
}

#[test]
fn materials_are_replaced_in_place_and_a_broken_replacement_keeps_the_old() -> TestResult {
    let ctx = noop()?;
    let mut rig = unlit_cube(&ctx, [0.9, 0.1, 0.1], 0.0)?;
    let m = mantis_render::scene::MaterialId(0);
    let blue = colored(flat(LightingModel::Lambert, None, true)?, [0.1, 0.1, 0.9]);
    let errors = validation_errors(&ctx.device, || {
        assert!(
            rig.renderer
                .replace_material(&ctx.device, &ctx.queue, m, blue, [None; 4])
                .is_ok()
        );
    });
    assert_eq!(errors, None);
    assert_eq!(rig.renderer.materials().len(), 1, "replaced, not added");
    // Dropping a deformation the material supports is refused; the material stays.
    let mut narrower = colored(flat(LightingModel::Unlit, None, false)?, [0.1, 0.9, 0.1]);
    narrower.deformations = mantis_formats::material::Deformations::NONE;
    assert_eq!(
        rig.renderer
            .replace_material(&ctx.device, &ctx.queue, m, narrower, [None; 4])
            .err(),
        Some(RendererError::Material(MaterialLoadError::Incompatible))
    );
    let unknown = mantis_render::scene::MaterialId(9);
    assert!(
        rig.renderer
            .replace_material(
                &ctx.device,
                &ctx.queue,
                unknown,
                flat(LightingModel::Unlit, None, false)?,
                [None; 4]
            )
            .is_err()
    );
    let Some(hw) = hardware_or_skip("materials_are_replaced_in_place_and_a_broken_replacement_keeps_the_old")
    else {
        return Ok(());
    };
    let mut rig = unlit_cube(&hw, [0.9, 0.1, 0.1], 0.0)?;
    let frame = inputs(front_camera());
    let red = pixel(&render(&hw, &mut rig.renderer, &frame)?, SIZE / 2, SIZE / 2);
    assert!(red[0] > red[2].saturating_add(80), "{red:?}");
    rig.renderer.replace_material(
        &hw.device,
        &hw.queue,
        m,
        colored(flat(LightingModel::Unlit, None, false)?, [0.1, 0.1, 0.9]),
        [None; 4],
    )?;
    let blue = pixel(&render(&hw, &mut rig.renderer, &frame)?, SIZE / 2, SIZE / 2);
    assert!(
        blue[2] > blue[0].saturating_add(80),
        "the same instance draws the new material: {blue:?}"
    );
    Ok(())
}
