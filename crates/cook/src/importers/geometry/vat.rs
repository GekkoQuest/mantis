//! Vertex animation bakes: `*.vat.toml` to an MVAT payload.
//!
//! The MVAT format itself lives in [`mantis_formats::vat`] (re-exported here).

use mantis_anim::{AnimError, Clip, Skeleton, SkinnedMesh, VatData, bake_vat};
use mantis_formats::FormatError;
use mantis_formats::anim_clip::ClipAsset;
use mantis_formats::bundle::{AssetKind, Domain};
use mantis_formats::mesh::MeshAsset;

use super::fields::{Doc, has_suffix, output_name};
use super::skeleton::resolve_skeleton;
use crate::importer::{CookError, Cooked, ImportContext, Importer, Source};

pub use mantis_formats::vat::{FLAG_LOOPING, MAGIC, MAX_TEXELS, VERSION, VatAsset};

/// The payload of a bake.
pub fn vat_asset(v: VatData) -> VatAsset {
    VatAsset {
        vertex_count: v.vertex_count,
        frame_count: v.frame_count,
        seconds_per_frame: v.seconds_per_frame,
        looping: v.looping,
        bounds_min: v.bounds_min,
        bounds_max: v.bounds_max,
        positions: v.positions,
        normals: v.normals,
    }
}

/// The inputs [`bake_vat`] takes, from a cooked skinned mesh (weights back to `0..=1`).
///
/// # Errors
/// [`FormatError::Inconsistent`] when the mesh has no skinning stream.
pub fn skinned_input(mesh: &MeshAsset) -> Result<SkinnedInput, FormatError> {
    let skin = mesh.skin.as_ref().ok_or(FormatError::Inconsistent)?;
    Ok(SkinnedInput {
        positions: mesh.vertices.iter().map(|v| v.position).collect(),
        normals: mesh.vertices.iter().map(|v| v.normal).collect(),
        joints: skin.iter().map(|s| s.joints).collect(),
        weights: skin
            .iter()
            .map(|s| s.weights.map(|w| f32::from(w) / 255.0))
            .collect(),
    })
}

/// Owned attribute arrays for a [`SkinnedMesh`].
#[derive(Clone, PartialEq, Debug, Default)]
pub struct SkinnedInput {
    /// Bind positions.
    pub positions: Vec<[f32; 3]>,
    /// Bind normals.
    pub normals: Vec<[f32; 3]>,
    /// Bone indices.
    pub joints: Vec<[u16; 4]>,
    /// Weights, `0..=1`.
    pub weights: Vec<[f32; 4]>,
}

impl SkinnedInput {
    /// The borrowed view [`bake_vat`] takes.
    pub fn mesh(&self) -> SkinnedMesh<'_> {
        SkinnedMesh {
            positions: &self.positions,
            normals: &self.normals,
            joints: &self.joints,
            weights: &self.weights,
        }
    }
}

/// `*.vat.toml` to an MVAT bake (phase 15: it resolves a phase-10 clip).
#[derive(Clone, Copy, Debug, Default)]
pub struct VatImporter;

impl Importer for VatImporter {
    fn name(&self) -> &'static str {
        "vat.bake"
    }

    fn version(&self) -> u32 {
        1
    }

    fn phase(&self) -> u32 {
        15
    }

    fn accepts(&self, path: &str) -> bool {
        has_suffix(path, ".vat.toml")
    }

    fn import(&self, source: &Source<'_>, ctx: &ImportContext<'_>) -> Result<Vec<Cooked>, CookError> {
        let doc = Doc::parse(source)?;
        doc.only_tables(&[], &[])?;
        let root = doc.root();
        root.only(&["skeleton", "clip", "mesh", "fps"])?;
        let skeleton = resolve_skeleton(ctx, &root, source.path, "skeleton")?;
        let skeleton_line = root.line_of("skeleton");
        let skeleton = Skeleton::new(&skeleton)
            .map_err(|e| root.err(skeleton_line, &format!("the skeleton does not bind: {e}")))?;
        let clip_path = root.str("clip")?;
        let clip_line = root.line_of("clip");
        let (_, clip_bytes) = ctx.resolve_bytes(clip_path, AssetKind::AnimClip, source.path, clip_line)?;
        let clip = ClipAsset::parse(clip_bytes)
            .map_err(AnimError::from)
            .and_then(|c| Clip::new(&c))
            .map_err(|e| root.err(clip_line, &format!("`{clip_path}`: {e}")))?;
        let mesh_path = root.str("mesh")?;
        let mesh_line = root.line_of("mesh");
        let (_, mesh_bytes) = ctx.resolve_bytes(mesh_path, AssetKind::Mesh, source.path, mesh_line)?;
        let mesh =
            MeshAsset::parse(mesh_bytes).map_err(|e| root.err(mesh_line, &format!("`{mesh_path}`: {e}")))?;
        let input = skinned_input(&mesh)
            .map_err(|_| root.err(mesh_line, &format!("`{mesh_path}` is not a skinned mesh")))?;
        let fps = root.f32("fps")?;
        let fps_line = root.line_of("fps");
        let vat = bake_vat(&skeleton, &clip, &input.mesh(), fps).map_err(|e| match e {
            AnimError::BoneCountMismatch { expected, actual } => root.err(
                clip_line,
                &format!("`{clip_path}` targets {actual} bones; the skeleton has {expected}"),
            ),
            AnimError::InvalidFrameRate => root.err(fps_line, "`fps` must be positive"),
            AnimError::VatTooLarge => root.err(fps_line, &format!("the bake exceeds {MAX_TEXELS} texels")),
            AnimError::InvalidMesh(v) => root.err(
                mesh_line,
                &format!("`{mesh_path}` vertex {v} does not skin on this skeleton"),
            ),
            other => root.err(0, &format!("the bake failed: {other}")),
        })?;
        let bytes = vat_asset(vat).encode();
        VatAsset::parse(&bytes)
            .map_err(|e| CookError::at(source.path, 0, &format!("the cooked bake does not load: {e}")))?;
        Ok(vec![Cooked {
            name: output_name(source.path, ".vat.toml", ".vat"),
            kind: AssetKind::VertexAnimation,
            domain: Domain::Presentation,
            bytes,
        }])
    }
}
