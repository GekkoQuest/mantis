//! Unloading meshes and materials: free lists return their space, ids are reused, a mesh
//! or material in use is refused, and frames stay valid after removals (no-op backend).

use glam::{Mat4, Vec3};
use mantis_formats::material::{
    ColorDefault, Deformations, LightingModel, MaterialAsset, MaterialBindings, MaterialGraph, Node, NodeId,
    SurfaceOutputs,
};
use mantis_formats::sh::ShL1;
use mantis_render::gpu::GpuContext;
use mantis_render::gpu_test::{noop, validation_errors};
use mantis_render::math::Camera;
use mantis_render::mesh::cube;
use mantis_render::post::PostSettings;
use mantis_render::renderer::{FrameInputs, Renderer, RendererConfig, RendererError};
use mantis_render::scene::SceneError;

type TestResult = Result<(), Box<dyn std::error::Error>>;
type Error = Box<dyn std::error::Error>;

fn material() -> Result<MaterialAsset, Error> {
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
        casts_shadows: true,
        deformations: Deformations::NONE,
        bindings: MaterialBindings {
            textures: Vec::new(),
            scalars: Vec::new(),
            colors: vec![ColorDefault {
                name: "color".to_owned(),
                value: [0.5, 0.5, 0.5, 1.0],
            }],
        },
    })
}

fn frame(ctx: &GpuContext, r: &mut Renderer) -> Result<(), Error> {
    let _ = r.prepare(&FrameInputs {
        camera: Camera {
            position: Vec3::new(0.0, 0.0, -4.0),
            yaw: 0.0,
            pitch: 0.0,
            fov_y: 1.0,
            aspect: 1.0,
            near: 0.1,
        },
        time: 0.0,
        sun_direction: Vec3::NEG_Y,
        sun_color: Vec3::ONE,
        shadows: true,
        sky: ShL1::ZERO,
        ambient_intensity: 1.0,
        exposure: 1.0,
        clear_color: [0.0; 4],
        ssao_strength: 0.5,
        post: PostSettings::default(),
    });
    let t = ctx.device.create_texture(&wgpu::TextureDescriptor {
        label: None,
        size: wgpu::Extent3d {
            width: 32,
            height: 32,
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

#[test]
fn meshes_and_materials_unload_into_free_lists() -> TestResult {
    let ctx = noop()?;
    let mut result: Result<(), Error> = Ok(());
    let errors = validation_errors(&ctx.device, || {
        result = (|| {
            let bindless = ctx.capabilities.bindless_textures;
            let mut config = RendererConfig::new(32, 32, wgpu::TextureFormat::Rgba8Unorm);
            config.max_vertices = 24 * 3;
            config.max_indices = 36 * 3;
            config.shadow_resolution = 256;
            let mut rend = Renderer::new(&ctx.device, &ctx.queue, bindless, config)?;
            let empty = rend.mesh_space();
            let (verts, idx) = cube();
            let texture = mantis_render::textures::solid_2d(&ctx.device, &ctx.queue, "t", [9, 9, 9, 255]);
            // Fill the mesh store, then churn: every lap removes and re-adds.
            let mesh_a = rend.add_mesh(&ctx.queue, &verts, &idx)?;
            let mesh_b = rend.add_mesh(&ctx.queue, &verts, &idx)?;
            let mat = rend.add_material(
                &ctx.device,
                &ctx.queue,
                material()?,
                [Some(&texture), None, None, None],
            )?;
            let inst = rend.scene_mut().spawn(mesh_a, mat, Mat4::IDENTITY)?;
            frame(&ctx, &mut rend)?;
            assert!(matches!(
                rend.remove_mesh(mesh_a),
                Err(RendererError::Scene(SceneError::InUse))
            ));
            assert!(matches!(
                rend.remove_material(&ctx.device, mat),
                Err(RendererError::Scene(SceneError::InUse))
            ));
            rend.despawn(inst)?;
            for _ in 0..20 {
                rend.remove_mesh(mesh_a)?;
                let again = rend.add_mesh(&ctx.queue, &verts, &idx)?;
                assert_eq!(again, mesh_a, "the freed id is reused");
                rend.remove_material(&ctx.device, mat)?;
                assert_eq!(rend.materials().len(), 0);
                let m2 = rend.add_material(
                    &ctx.device,
                    &ctx.queue,
                    material()?,
                    [Some(&texture), None, None, None],
                )?;
                assert_eq!(m2, mat, "the freed material slot is reused");
                let inst = rend.scene_mut().spawn(again, m2, Mat4::IDENTITY)?;
                frame(&ctx, &mut rend)?;
                rend.despawn(inst)?;
            }
            // The store never grew: a third mesh still fits, a fourth does not.
            let mesh_c = rend.add_mesh(&ctx.queue, &verts, &idx)?;
            assert!(
                rend.add_mesh(&ctx.queue, &verts, &idx).is_err(),
                "store full at three meshes"
            );
            for mesh in [mesh_a, mesh_b, mesh_c] {
                rend.remove_mesh(mesh)?;
            }
            assert_eq!(rend.mesh_space(), empty, "every range returned");
            assert_eq!(rend.scene_mut().mesh_count(), 0);
            frame(&ctx, &mut rend)?;
            Ok(())
        })();
    });
    result?;
    assert_eq!(errors, None);
    Ok(())
}
