//! Small texture helpers: fallbacks for unbound slots and uploads of cooked data.

use mantis_formats::probe_volume::ProbeVolume;
use mantis_formats::sh::ShL1;

use crate::lighting::indirect::blend_probes;

fn write(
    queue: &wgpu::Queue,
    texture: &wgpu::Texture,
    bytes: &[u8],
    bytes_per_row: u32,
    rows: u32,
    size: wgpu::Extent3d,
) {
    queue.write_texture(
        texture.as_image_copy(),
        bytes,
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(bytes_per_row),
            rows_per_image: Some(rows),
        },
        size,
    );
}

/// A 1 x 1 RGBA8 texture of one color.
pub fn solid_2d(device: &wgpu::Device, queue: &wgpu::Queue, label: &str, rgba: [u8; 4]) -> wgpu::TextureView {
    let size = wgpu::Extent3d {
        width: 1,
        height: 1,
        depth_or_array_layers: 1,
    };
    let t = device.create_texture(&wgpu::TextureDescriptor {
        label: Some(label),
        size,
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8Unorm,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    write(queue, &t, &rgba, 4, 1, size);
    t.create_view(&wgpu::TextureViewDescriptor::default())
}

/// The probe volume as three RGBA16F 3D textures (red, green, blue coefficients), blended
/// to `time`. Invalid probes are dilated first: each takes the average of its valid
/// neighbors, so hardware trilinear filtering never blends in light from inside geometry.
#[derive(Debug)]
pub struct ProbeTextures {
    /// Red, green, blue coefficient volumes.
    pub views: [wgpu::TextureView; 3],
    /// The textures.
    pub textures: [wgpu::Texture; 3],
}

#[expect(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // Grid indices are bounded by the dimensions.
pub(crate) fn dilate(volume: &ProbeVolume, probes: &mut [ShL1]) {
    let Some(valid) = &volume.valid else { return };
    let [nx, ny, nz] = volume.dims.map(i64::from);
    let source: Vec<ShL1> = probes.to_vec();
    for z in 0..nz {
        for y in 0..ny {
            for x in 0..nx {
                let i = volume.index(x as u32, y as u32, z as u32);
                if valid.get(i).copied().unwrap_or(true) {
                    continue;
                }
                let mut acc = ShL1::ZERO;
                let mut n = 0.0f32;
                for (dx, dy, dz) in [
                    (-1, 0, 0),
                    (1, 0, 0),
                    (0, -1, 0),
                    (0, 1, 0),
                    (0, 0, -1),
                    (0, 0, 1),
                ] {
                    let (px, py, pz) = (x + dx, y + dy, z + dz);
                    if px < 0 || py < 0 || pz < 0 || px >= nx || py >= ny || pz >= nz {
                        continue;
                    }
                    let j = volume.index(px as u32, py as u32, pz as u32);
                    if valid.get(j).copied().unwrap_or(false)
                        && let Some(s) = source.get(j)
                    {
                        s.accumulate(&mut acc, 1.0);
                        n += 1.0;
                    }
                }
                if n > 0.0
                    && let Some(slot) = probes.get_mut(i)
                {
                    *slot = ShL1::ZERO;
                    acc.accumulate(slot, 1.0 / n);
                }
            }
        }
    }
}

impl ProbeTextures {
    /// Uploads a volume blended to `time`.
    pub fn upload(device: &wgpu::Device, queue: &wgpu::Queue, volume: &ProbeVolume, time: f32) -> Self {
        let mut blended = Vec::new();
        blend_probes(volume, time, &mut blended);
        dilate(volume, &mut blended);
        let [nx, ny, nz] = volume.dims;
        let size = wgpu::Extent3d {
            width: nx,
            height: ny,
            depth_or_array_layers: nz,
        };
        let make = |channel: usize| {
            let t = device.create_texture(&wgpu::TextureDescriptor {
                label: Some("probes"),
                size,
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D3,
                format: wgpu::TextureFormat::Rgba16Float,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            });
            let mut bytes = Vec::with_capacity(blended.len() * 8);
            for sh in &blended {
                for c in sh.rgb.get(channel).copied().unwrap_or([0.0; 4]) {
                    bytes.extend_from_slice(&mantis_formats::half::f32_to_f16(c).to_le_bytes());
                }
            }
            write(queue, &t, &bytes, nx * 8, ny, size);
            t
        };
        let textures = [make(0), make(1), make(2)];
        let views = textures
            .each_ref()
            .map(|t| t.create_view(&wgpu::TextureViewDescriptor::default()));
        Self { views, textures }
    }

    /// A 2 x 2 x 2 volume of zero light (bound when no volume is loaded).
    pub fn empty(device: &wgpu::Device, queue: &wgpu::Queue) -> Self {
        let volume = ProbeVolume {
            dims: [2, 2, 2],
            origin: [0.0; 3],
            spacing: [1.0; 3],
            keyframes: vec![0.0],
            probes: vec![vec![ShL1::ZERO; 8]],
            valid: None,
        };
        Self::upload(device, queue, &volume, 0.0)
    }
}
