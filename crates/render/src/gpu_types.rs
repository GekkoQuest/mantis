//! Rust mirrors of the WGSL structs in `mantis_shadergen::COMMON_WGSL` and the lighting
//! library. Every mirror is `#[repr(C)]`, `Pod`, and built only from 16-byte-aligned
//! members, so its byte layout equals the WGSL uniform and storage layout; a test checks
//! each size against naga's layouter.

use bytemuck::{Pod, Zeroable};

/// The `Frame` uniform.
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Debug, Pod, Zeroable)]
pub struct GpuFrame {
    /// World to view.
    pub view: [[f32; 4]; 4],
    /// View to clip.
    pub proj: [[f32; 4]; 4],
    /// World to clip.
    pub view_proj: [[f32; 4]; 4],
    /// Clip to world.
    pub inv_view_proj: [[f32; 4]; 4],
    /// The previous frame's unjittered world to clip (motion vectors).
    pub prev_view_proj: [[f32; 4]; 4],
    /// Camera position, w = seconds.
    pub camera_position: [f32; 4],
    /// Width, height, 1 / width, 1 / height.
    pub viewport: [f32; 4],
    /// Direction sunlight travels.
    pub sun_direction: [f32; 4],
    /// Sun color times intensity, w = shadows enabled.
    pub sun_color: [f32; 4],
    /// Sky ambient L1 SH (red, green, blue).
    pub sky_sh: [[f32; 4]; 3],
    /// Cascade matrices.
    pub cascade_view_proj: [[[f32; 4]; 4]; 4],
    /// Cascade far depths.
    pub cascade_splits: [f32; 4],
    /// Count, depth bias, normal offset, PCF radius.
    pub cascade_params: [f32; 4],
    /// Tiles across, down, slices, light count.
    pub cluster_dims: [u32; 4],
    /// Near, far, slice scale, tile width.
    pub cluster_depth: [f32; 4],
    /// Tile height.
    pub cluster_tile: [f32; 4],
    /// 1 / sector size, probe sector table side, 1 when any probes are loaded, unused.
    pub probe_grid: [f32; 4],
    /// 1 / probe atlas size per axis, w = ambient intensity.
    pub probe_atlas: [f32; 4],
    /// Reserved.
    pub indirect_params: [f32; 4],
}

/// The `PassView` uniform (bound with a dynamic offset, so padded to 256 bytes).
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Debug, Pod, Zeroable)]
pub struct GpuPassView {
    /// World to clip for this pass.
    pub view_proj: [[f32; 4]; 4],
    /// Eye position.
    pub position: [f32; 4],
    /// Pass-specific values.
    pub params: [f32; 4],
    /// Padding to the dynamic-offset alignment.
    pub pad: [[f32; 4]; 10],
}

/// The `DrawParams` uniform (dynamic offset, padded to 256 bytes).
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq, Debug, Pod, Zeroable)]
pub struct GpuDrawParams {
    /// Visible-list base of the batch.
    pub base: u32,
    /// Padding.
    pub pad: [u32; 63],
}

/// The `MaterialParams` uniform.
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Debug, Pod, Zeroable)]
pub struct GpuMaterialParams {
    /// Scalar parameters, four per vector.
    pub scalars: [[f32; 4]; 4],
    /// Color parameters.
    pub colors: [[f32; 4]; 8],
    /// Bindless table index of each texture slot.
    pub texture_index: [u32; 4],
}

impl GpuMaterialParams {
    /// The parameters at a material's defaults.
    pub fn from_defaults(bindings: &mantis_formats::material::MaterialBindings) -> Self {
        let mut p = Self::default();
        for (i, s) in bindings.scalars.iter().enumerate() {
            if let Some(slot) = p.scalars.get_mut(i / 4).and_then(|row| row.get_mut(i % 4)) {
                *slot = s.value;
            }
        }
        for (slot, c) in p.colors.iter_mut().zip(&bindings.colors) {
            *slot = c.value;
        }
        p
    }
}

impl Default for GpuMaterialParams {
    fn default() -> Self {
        Self {
            scalars: [[0.0; 4]; 4],
            colors: [[1.0; 4]; 8],
            texture_index: [0; 4],
        }
    }
}

/// A vertex in the shared vertex buffer.
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Debug, Pod, Zeroable)]
pub struct Vertex {
    /// Position.
    pub position: [f32; 3],
    /// Normal.
    pub normal: [f32; 3],
    /// Texture coordinates.
    pub uv0: [f32; 2],
    /// Lightmap coordinates.
    pub uv1: [f32; 2],
}

/// Vertex buffer layout matching `VertexIn`.
pub const VERTEX_ATTRIBUTES: [wgpu::VertexAttribute; 4] = wgpu::vertex_attr_array![
    0 => Float32x3,
    1 => Float32x3,
    2 => Float32x2,
    3 => Float32x2,
];

/// The vertex buffer layout.
pub fn vertex_layout() -> wgpu::VertexBufferLayout<'static> {
    wgpu::VertexBufferLayout {
        array_stride: core::mem::size_of::<Vertex>() as u64,
        step_mode: wgpu::VertexStepMode::Vertex,
        attributes: &VERTEX_ATTRIBUTES,
    }
}

/// The skinning stream of a skinned mesh, parallel to its vertices (same base vertex):
/// four bone indices and four weights (renormalized in the shader).
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Pod, Zeroable)]
pub struct SkinVertex {
    /// Bone indices.
    pub joints: [u16; 4],
    /// Weights, 0 to 255.
    pub weights: [u8; 4],
}

/// Skinning stream attributes (`VertexIn.joints`, `VertexIn.weights` of skinned
/// permutations).
pub const SKIN_ATTRIBUTES: [wgpu::VertexAttribute; 2] = [
    wgpu::VertexAttribute {
        format: wgpu::VertexFormat::Uint16x4,
        offset: 0,
        shader_location: mantis_shadergen::layout::SKIN_JOINTS_LOCATION,
    },
    wgpu::VertexAttribute {
        format: wgpu::VertexFormat::Unorm8x4,
        offset: 8,
        shader_location: mantis_shadergen::layout::SKIN_JOINTS_LOCATION + 1,
    },
];

/// Vertex buffer layout of the skinning stream (slot 1 of skinned pipelines).
pub fn skin_layout() -> wgpu::VertexBufferLayout<'static> {
    wgpu::VertexBufferLayout {
        array_stride: core::mem::size_of::<SkinVertex>() as u64,
        step_mode: wgpu::VertexStepMode::Vertex,
        attributes: &SKIN_ATTRIBUTES,
    }
}

/// Front faces are clockwise on screen: the world is left-handed (decision 0019), so a
/// triangle whose cross product `(b - a) x (c - a)` points outward appears clockwise from
/// outside. Every pipeline uses this.
pub const FRONT_FACE: wgpu::FrontFace = wgpu::FrontFace::Cw;

/// Dynamic-offset stride for per-pass and per-draw uniforms.
pub const DYNAMIC_STRIDE: u64 = 256;

#[cfg(test)]
mod tests {
    use super::*;
    use mantis_formats::material::{Pass, reference_materials};
    use wgpu::naga;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    /// Sizes of named WGSL structs in a full forward permutation, per naga's layouter.
    fn wgsl_sizes() -> Result<Vec<(String, u32)>, Box<dyn std::error::Error>> {
        let refs = reference_materials()?;
        let toon = refs.first().ok_or("toon")?;
        let key = toon.request(Pass::Forward, false, false).ok_or("key")?;
        let p = mantis_shadergen::compile(toon, key)?;
        let module = naga::front::wgsl::parse_str(&p.source).map_err(|e| e.emit_to_string(&p.source))?;
        let mut layouter = naga::proc::Layouter::default();
        layouter.update(module.to_ctx()).map_err(|e| format!("{e:?}"))?;
        Ok(module
            .types
            .iter()
            .filter_map(|(handle, ty)| Some((ty.name.clone()?, layouter[handle].size)))
            .collect())
    }

    #[test]
    fn mirrors_match_wgsl_layouts() -> TestResult {
        let sizes = wgsl_sizes()?;
        let size = |name: &str| sizes.iter().find(|(n, _)| n == name).map(|(_, s)| *s as usize);
        assert_eq!(size("Frame"), Some(core::mem::size_of::<GpuFrame>()));
        assert_eq!(
            size("MaterialParams"),
            Some(core::mem::size_of::<GpuMaterialParams>())
        );
        assert_eq!(
            size("Instance"),
            Some(core::mem::size_of::<crate::scene::GpuInstance>())
        );
        assert_eq!(
            size("PointLight"),
            Some(core::mem::size_of::<crate::lighting::clusters::GpuPointLight>())
        );
        // Dynamic-offset uniforms are padded to the stride; the WGSL struct is a prefix.
        assert!(size("PassView").is_some_and(|s| s <= core::mem::size_of::<GpuPassView>()));
        assert!(size("DrawParams").is_some_and(|s| s <= core::mem::size_of::<GpuDrawParams>()));
        assert_eq!(core::mem::size_of::<GpuPassView>() as u64, DYNAMIC_STRIDE);
        assert_eq!(core::mem::size_of::<GpuDrawParams>() as u64, DYNAMIC_STRIDE);
        assert_eq!(core::mem::size_of::<Vertex>(), 40);
        Ok(())
    }
}
