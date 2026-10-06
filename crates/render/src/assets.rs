//! Uploads of cooked assets: MMSH meshes into the shared vertex and index buffers, and
//! MTEX textures into GPU textures (block-compressed when the device supports it).

use mantis_formats::mesh::MeshAsset;
use mantis_formats::texture::{Encoding, FLAG_SRGB, TextureAsset, mip_size};

use crate::gpu::Capabilities;
use crate::gpu_types::{SkinVertex, Vertex};

/// Why a cooked asset could not be uploaded.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AssetError {
    /// The device cannot sample this encoding (block compression unsupported).
    Unsupported(Encoding),
    /// Block-compressed textures need dimensions that are multiples of four.
    BlockDimensions,
    /// The payload disagrees with itself (mip sizes); it should have failed parsing.
    Malformed,
}

impl core::fmt::Display for AssetError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            AssetError::Unsupported(e) => write!(f, "texture encoding {e:?} unsupported by the device"),
            AssetError::BlockDimensions => f.write_str("block-compressed texture size not a multiple of 4"),
            AssetError::Malformed => f.write_str("malformed texture payload"),
        }
    }
}

impl std::error::Error for AssetError {}

/// A cooked mesh's vertices in the renderer's layout.
pub fn mesh_vertices(mesh: &MeshAsset) -> Vec<Vertex> {
    mesh.vertices
        .iter()
        .map(|v| Vertex {
            position: v.position,
            normal: v.normal,
            uv0: v.uv0,
            uv1: v.uv1,
        })
        .collect()
}

/// A cooked mesh's skinning stream in the renderer's layout, when it has one.
pub fn mesh_skin(mesh: &MeshAsset) -> Option<Vec<SkinVertex>> {
    mesh.skin.as_ref().map(|skin| {
        skin.iter()
            .map(|s| SkinVertex {
                joints: s.joints,
                weights: s.weights,
            })
            .collect()
    })
}

/// The GPU format of an encoding.
pub fn texture_format(encoding: Encoding, srgb: bool) -> wgpu::TextureFormat {
    use wgpu::TextureFormat as F;
    match (encoding, srgb) {
        (Encoding::Bc1, false) => F::Bc1RgbaUnorm,
        (Encoding::Bc1, true) => F::Bc1RgbaUnormSrgb,
        (Encoding::Bc4, _) => F::Bc4RUnorm,
        (Encoding::Bc5, _) => F::Bc5RgUnorm,
        (Encoding::Bc7, false) => F::Bc7RgbaUnorm,
        (Encoding::Bc7, true) => F::Bc7RgbaUnormSrgb,
        (Encoding::Rgba8, false) => F::Rgba8Unorm,
        (Encoding::Rgba8, true) => F::Rgba8UnormSrgb,
    }
}

/// Uploads every mip of a cooked texture.
///
/// # Errors
/// [`AssetError::Unsupported`] for block compression on a device without it,
/// [`AssetError::BlockDimensions`], or [`AssetError::Malformed`].
pub fn upload_texture(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    capabilities: Capabilities,
    texture: &TextureAsset,
) -> Result<(wgpu::Texture, wgpu::TextureView), AssetError> {
    let compressed = texture.encoding != Encoding::Rgba8;
    if compressed && !capabilities.compressed_textures {
        return Err(AssetError::Unsupported(texture.encoding));
    }
    if compressed && (!texture.width.is_multiple_of(4) || !texture.height.is_multiple_of(4)) {
        return Err(AssetError::BlockDimensions);
    }
    let levels = u32::try_from(texture.mips.len()).map_err(|_| AssetError::Malformed)?;
    if levels == 0 {
        return Err(AssetError::Malformed);
    }
    let format = texture_format(texture.encoding, texture.flags & FLAG_SRGB != 0);
    let t = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("cooked.texture"),
        size: wgpu::Extent3d {
            width: texture.width,
            height: texture.height,
            depth_or_array_layers: 1,
        },
        mip_level_count: levels,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    for (level, bytes) in (0u32..).zip(&texture.mips) {
        let (w, h) = mip_size(texture.width, texture.height, level);
        if bytes.len() as u64 != texture.encoding.image_bytes(w, h) {
            return Err(AssetError::Malformed);
        }
        // Block-compressed copies cover whole blocks (the physical mip size).
        let (copy_w, copy_h, row, rows) = if compressed {
            let block = if matches!(texture.encoding, Encoding::Bc1 | Encoding::Bc4) {
                8
            } else {
                16
            };
            let (bw, bh) = (w.div_ceil(4), h.div_ceil(4));
            (bw * 4, bh * 4, bw * block, bh)
        } else {
            (w, h, w * 4, h)
        };
        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &t,
                mip_level: level,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            bytes,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(row),
                rows_per_image: Some(rows),
            },
            wgpu::Extent3d {
                width: copy_w,
                height: copy_h,
                depth_or_array_layers: 1,
            },
        );
    }
    let view = t.create_view(&wgpu::TextureViewDescriptor::default());
    Ok((t, view))
}
