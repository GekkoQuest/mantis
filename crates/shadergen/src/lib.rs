//! mantis-shadergen: compiles material graphs (`mantis_formats::material`) into WGSL
//! permutations, as text (decision 0018).
//!
//! GPU-free: the cook compiles every canonical permutation offline; render compiles at
//! development time for hot reload. Each permutation is one complete WGSL module:
//!
//! 1. feature constants (`ALPHA_TEST`, `LIGHTMAPPED`, `MATERIAL_UNLIT`, outline settings);
//! 2. the material's texture declarations (fixed slots or a bindless array);
//! 3. the shared declarations ([`COMMON_WGSL`]) and the deformation's vertex input and
//!    `deform_local` ([`DEFORM_STATIC_WGSL`], [`DEFORM_SKINNED_WGSL`], [`DEFORM_VAT_WGSL`]);
//! 4. the generated material functions: `material_surface`, `material_diffuse` (the
//!    direct-light response, where toon banding and ramps live), and `material_rim`;
//! 5. for the forward pass, the lighting library ([`LIGHTING_WGSL`]);
//! 6. the pass's entry points (`vs_main`, and `fs_main` where the pass has a fragment
//!    stage).
//!
//! Binding layout (shared with render's pipeline layouts, see [`layout`]): group 0 frame,
//! pass view, and in the forward pass the lighting resources; group 1 the GPU-driven
//! scene; group 2 the material; in bindless permutations, group 3 the global texture
//! table, indexed through the material's `texture_index`.

#![forbid(unsafe_code)]

mod codegen;

pub use mantis_formats::material::{Deform, MaterialAsset, Pass, PermutationKey};

/// Shared declarations.
pub const COMMON_WGSL: &str = include_str!("wgsl/common.wgsl");
/// The forward lighting library.
pub const LIGHTING_WGSL: &str = include_str!("wgsl/lighting.wgsl");
/// Forward pass entry points.
pub const FORWARD_WGSL: &str = include_str!("wgsl/forward.wgsl");
/// Depth and shadow pass entry points.
pub const DEPTH_WGSL: &str = include_str!("wgsl/depth.wgsl");
/// Outline pass entry points.
pub const OUTLINE_WGSL: &str = include_str!("wgsl/outline.wgsl");
/// Vertex input and `deform_local` for static geometry.
pub const DEFORM_STATIC_WGSL: &str = include_str!("wgsl/deform/static.wgsl");
/// Vertex input and `deform_local` for skinned meshes.
pub const DEFORM_SKINNED_WGSL: &str = include_str!("wgsl/deform/skinned.wgsl");
/// Vertex input and `deform_local` for vertex animation.
pub const DEFORM_VAT_WGSL: &str = include_str!("wgsl/deform/vat.wgsl");

/// Binding numbers shared by the generated shaders and render's pipeline layouts.
pub mod layout {
    /// Frame uniform, pass view (dynamic offset), forward lighting resources.
    pub const GROUP_FRAME: u32 = 0;
    /// Instances, visible list, draw parameters (dynamic offset), deformation pool.
    pub const GROUP_SCENE: u32 = 1;
    /// Scene-group binding of the deformation pool (bone palettes and vertex animations).
    pub const DEFORM_POOL_BINDING: u32 = 3;
    /// Scene-group binding of every instance's previous-frame model matrix (indexed like
    /// the instances), for motion vectors.
    pub const PREVIOUS_MODELS_BINDING: u32 = 4;
    /// Vertex stream of skinned meshes: joints `Uint16x4` at this location, weights
    /// `Unorm8x4` at the next, 12 bytes per vertex.
    pub const SKIN_JOINTS_LOCATION: u32 = 4;
    /// Material parameters, sampler, textures.
    pub const GROUP_MATERIAL: u32 = 2;
    /// First material texture binding in fixed-slot permutations (this and the next three).
    pub const MATERIAL_TEXTURE_BINDING: u32 = 2;
    /// Bindless permutations: the global texture table, shared by every material (a
    /// binding array may not share a bind group with uniform buffers).
    pub const GROUP_TEXTURES: u32 = 3;
    /// Size of the bindless texture array.
    pub const BINDLESS_TEXTURES: u32 = 1024;
    /// Group-0 binding of the screen-space ambient occlusion texture.
    pub const SSAO_BINDING: u32 = 12;
    /// Group-0 binding of the lightmap page table: per page, the two keyframe layers of
    /// the page array to blend and the blend weight.
    pub const LIGHTMAP_PAGES_BINDING: u32 = 13;
    /// Group-0 binding of the probe sector table: a toroidal grid of world sectors, each
    /// naming its brick in the probe atlas.
    pub const PROBE_SECTORS_BINDING: u32 = 14;
}

/// One compiled permutation.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Permutation {
    /// Which permutation.
    pub key: PermutationKey,
    /// The complete WGSL module.
    pub source: String,
    /// Vertex entry point.
    pub vertex_entry: &'static str,
    /// Fragment entry point, when the pass has a fragment stage.
    pub fragment_entry: Option<&'static str>,
}

/// Compilation errors.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ShaderError {
    /// The key is not one of the material's canonical permutations.
    NotAPermutation(PermutationKey),
}

impl core::fmt::Display for ShaderError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            ShaderError::NotAPermutation(k) => write!(f, "{k:?} is not a permutation of this material"),
        }
    }
}

impl std::error::Error for ShaderError {}

/// Compiles one permutation.
///
/// # Errors
/// [`ShaderError::NotAPermutation`] if `key` is not among the material's canonical
/// permutations (the only ones the cook emits and the renderer requests).
pub fn compile(asset: &MaterialAsset, key: PermutationKey) -> Result<Permutation, ShaderError> {
    if !asset.canonical_permutations().contains(&key) {
        return Err(ShaderError::NotAPermutation(key));
    }
    let pass = key.pass();
    let mut source = String::new();
    codegen::constants(&mut source, asset, key);
    codegen::textures(&mut source, key.bindless());
    source.push_str(COMMON_WGSL);
    source.push_str(match key.deform() {
        Deform::Static => DEFORM_STATIC_WGSL,
        Deform::Skinned => DEFORM_SKINNED_WGSL,
        Deform::Vat => DEFORM_VAT_WGSL,
    });
    codegen::material_functions(&mut source, asset, key.bindless());
    let entries = match pass {
        Pass::Forward => {
            source.push_str(LIGHTING_WGSL);
            FORWARD_WGSL
        }
        Pass::Depth | Pass::Shadow => DEPTH_WGSL,
        Pass::Outline => OUTLINE_WGSL,
    };
    source.push_str(entries);
    let fragment_entry = match pass {
        Pass::Forward | Pass::Outline => Some("fs_main"),
        // The depth prepass always writes the velocity target; shadow maps need a
        // fragment stage only for alpha testing.
        Pass::Depth => Some("fs_velocity"),
        Pass::Shadow => key.alpha_test().then_some("fs_main"),
    };
    Ok(Permutation {
        key,
        source,
        vertex_entry: "vs_main",
        fragment_entry,
    })
}

/// Compiles every canonical permutation of a material.
///
/// # Errors
/// None in practice (every canonical key compiles); returned for uniformity.
pub fn compile_all(asset: &MaterialAsset) -> Result<Vec<Permutation>, ShaderError> {
    asset
        .canonical_permutations()
        .into_iter()
        .map(|k| compile(asset, k))
        .collect()
}
