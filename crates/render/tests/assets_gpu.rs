//! Uploads of cooked meshes and textures (API validation on the no-op backend).

use mantis_formats::mesh::{MeshAsset, MeshVertex, Meshlet, SkinInfluence};
use mantis_formats::texture::{Encoding, FLAG_SRGB, TextureAsset, full_chain, mip_size};
use mantis_render::assets::{AssetError, mesh_skin, mesh_vertices, texture_format, upload_texture};
use mantis_render::gpu::Capabilities;
use mantis_render::gpu_test::{noop, validation_errors};
use mantis_render::renderer::{Renderer, RendererConfig};

type TestResult = Result<(), Box<dyn std::error::Error>>;

fn chain(encoding: Encoding, width: u32, height: u32) -> TextureAsset {
    let mips = (0..full_chain(width, height))
        .map(|level| {
            let (w, h) = mip_size(width, height, level);
            vec![0x5a; usize::try_from(encoding.image_bytes(w, h)).unwrap_or(0)]
        })
        .collect();
    TextureAsset {
        encoding,
        flags: 0,
        encoder_version: u32::from(encoding != Encoding::Rgba8),
        width,
        height,
        mips,
    }
}

fn triangle(skinned: bool) -> MeshAsset {
    let v = |x: f32, y: f32| MeshVertex {
        position: [x, y, 0.0],
        normal: [0.0, 0.0, -1.0],
        uv0: [x, y],
        uv1: [x, y],
    };
    MeshAsset {
        vertices: vec![v(0.0, 0.0), v(0.0, 1.0), v(1.0, 0.0)],
        skin: skinned.then(|| {
            vec![
                SkinInfluence {
                    joints: [0, 1, 0, 0],
                    weights: [200, 55, 0, 0],
                };
                3
            ]
        }),
        indices: vec![0, 1, 2],
        meshlets: vec![Meshlet {
            first_index: 0,
            index_count: 3,
            center: [0.5, 0.5, 0.0],
            radius: 1.0,
            cone_axis: [0.0, 0.0, -1.0],
            cone_cutoff: 1.0,
        }],
        bounds_min: [0.0; 3],
        bounds_max: [1.0, 1.0, 0.0],
    }
}

#[test]
fn cooked_meshes_convert_and_upload_with_their_skinning_stream() -> TestResult {
    let ctx = noop()?;
    let plain = triangle(false);
    let skinned = triangle(true);
    let verts = mesh_vertices(&skinned);
    assert_eq!(verts.len(), 3);
    assert_eq!(verts.get(1).map(|v| v.uv1), Some([0.0, 1.0]));
    assert_eq!(mesh_skin(&plain), None);
    assert_eq!(
        mesh_skin(&skinned).and_then(|s| s.first().map(|k| (k.joints, k.weights))),
        Some(([0, 1, 0, 0], [200, 55, 0, 0]))
    );
    let mut config = RendererConfig::new(64, 64, wgpu::TextureFormat::Rgba8Unorm);
    config.max_vertices = 64;
    config.max_indices = 64;
    let mut r = Renderer::new(&ctx.device, &ctx.queue, false, config)?;
    let a = r.add_mesh_asset(&ctx.queue, &plain)?;
    let b = r.add_mesh_asset(&ctx.queue, &skinned)?;
    assert_ne!(a, b);
    Ok(())
}

#[test]
fn cooked_textures_upload_every_mip_or_refuse_cleanly() -> TestResult {
    let ctx = noop()?;
    let caps = ctx.capabilities;
    let mut outcome: Result<(), Box<dyn std::error::Error>> = Ok(());
    let errors = validation_errors(&ctx.device, || {
        outcome = (|| {
            let mut rgba = chain(Encoding::Rgba8, 6, 3);
            rgba.flags = FLAG_SRGB;
            let (t, _) = upload_texture(&ctx.device, &ctx.queue, caps, &rgba)?;
            assert_eq!(t.mip_level_count(), 3);
            assert_eq!(t.format(), wgpu::TextureFormat::Rgba8UnormSrgb);
            let without = Capabilities {
                compressed_textures: false,
                ..caps
            };
            for encoding in [Encoding::Bc1, Encoding::Bc4, Encoding::Bc5, Encoding::Bc7] {
                let bc = chain(encoding, 16, 8);
                assert_eq!(
                    upload_texture(&ctx.device, &ctx.queue, without, &bc).err(),
                    Some(AssetError::Unsupported(encoding))
                );
                if caps.compressed_textures {
                    let (t, _) = upload_texture(&ctx.device, &ctx.queue, caps, &bc)?;
                    assert_eq!(t.mip_level_count(), 5, "mips below a block upload whole blocks");
                    assert_eq!(t.format(), texture_format(encoding, false));
                    assert_eq!(
                        upload_texture(&ctx.device, &ctx.queue, caps, &chain(encoding, 6, 8)).err(),
                        Some(AssetError::BlockDimensions)
                    );
                }
            }
            let mut bad = chain(Encoding::Rgba8, 4, 4);
            if let Some(m) = bad.mips.get_mut(1) {
                m.pop();
            }
            assert_eq!(
                upload_texture(&ctx.device, &ctx.queue, caps, &bad).err(),
                Some(AssetError::Malformed)
            );
            Ok(())
        })();
    });
    outcome?;
    assert_eq!(errors, None);
    Ok(())
}
