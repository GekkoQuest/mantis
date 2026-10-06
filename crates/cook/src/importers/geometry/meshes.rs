//! The mesh importers: `*.obj` (static) and `*.skinmesh.toml` (skinned).

use mantis_formats::bundle::{AssetKind, Domain};
use mantis_formats::mesh::{MeshAsset, SkinInfluence};

use super::fields::{Doc, Fields, has_suffix, output_name};
use super::mesh::{MeshOptions, build, quantize_weights};
use super::obj::Obj;
use crate::importer::{CookError, Cooked, ImportContext, Importer, Source};

/// Tolerance on the sum of a vertex's authored weights.
const WEIGHT_SUM_TOLERANCE: f32 = 1e-3;

fn options(f: &Fields<'_>) -> Result<MeshOptions, CookError> {
    let scale = f.opt_f32("scale")?.unwrap_or(1.0);
    if scale <= 0.0 {
        return Err(f.err(f.line_of("scale"), "`scale` must be positive"));
    }
    Ok(MeshOptions {
        scale,
        lightmap_uv: f.opt_bool("lightmap_uv")?.unwrap_or(false),
    })
}

fn cooked(name: String, mesh: &MeshAsset) -> Vec<Cooked> {
    vec![Cooked {
        name,
        kind: AssetKind::Mesh,
        domain: Domain::Presentation,
        bytes: mesh.encode(),
    }]
}

/// `*.obj` to MMSH, with optional `<file>.obj.toml` settings. `*.skin.obj` files are
/// skinned-mesh geometry, read only through a `*.skinmesh.toml`.
#[derive(Clone, Copy, Debug, Default)]
pub struct ObjImporter;

impl Importer for ObjImporter {
    fn name(&self) -> &'static str {
        "mesh.obj"
    }

    fn version(&self) -> u32 {
        1
    }

    fn phase(&self) -> u32 {
        0
    }

    fn accepts(&self, path: &str) -> bool {
        has_suffix(path, ".obj") && !has_suffix(path, ".skin.obj")
    }

    fn inputs(&self, path: &str) -> bool {
        has_suffix(path, ".obj.toml") || has_suffix(path, ".skin.obj")
    }

    fn import(&self, source: &Source<'_>, ctx: &ImportContext<'_>) -> Result<Vec<Cooked>, CookError> {
        let sidecar_path = format!("{}.toml", source.path);
        let opts = match ctx.source(&sidecar_path) {
            Some(sidecar) => {
                let doc = Doc::parse(&sidecar)?;
                doc.only_tables(&[], &[])?;
                let root = doc.root();
                root.only(&["scale", "lightmap_uv"])?;
                options(&root)?
            }
            None => MeshOptions {
                scale: 1.0,
                lightmap_uv: false,
            },
        };
        let obj = Obj::parse(source)?;
        let mesh = build(source.path, &obj, opts, None)?;
        Ok(cooked(output_name(source.path, ".obj", ".mesh"), &mesh))
    }
}

/// `*.skinmesh.toml` to a skinned MMSH.
#[derive(Clone, Copy, Debug, Default)]
pub struct SkinnedMeshImporter;

impl Importer for SkinnedMeshImporter {
    fn name(&self) -> &'static str {
        "mesh.skinned"
    }

    fn version(&self) -> u32 {
        1
    }

    fn phase(&self) -> u32 {
        0
    }

    fn accepts(&self, path: &str) -> bool {
        has_suffix(path, ".skinmesh.toml")
    }

    fn import(&self, source: &Source<'_>, ctx: &ImportContext<'_>) -> Result<Vec<Cooked>, CookError> {
        let doc = Doc::parse(source)?;
        doc.only_tables(&["weights"], &[])?;
        let root = doc.root();
        root.only(&["mesh", "scale", "lightmap_uv"])?;
        let opts = options(&root)?;
        let obj_path = root.str("mesh")?;
        let mesh_line = root.line_of("mesh");
        let obj_source = ctx
            .source(obj_path)
            .ok_or_else(|| root.err(mesh_line, &format!("`{obj_path}` is not in the content tree")))?;
        let obj = Obj::parse(&obj_source)?;
        let mut skin: Vec<Option<SkinInfluence>> = vec![None; obj.positions.len()];
        for (rest, table) in doc.tables_under("weights") {
            let (index, influence) = weights_table(rest, &table, obj.positions.len())?;
            if let Some(slot) = skin.get_mut(index) {
                *slot = Some(influence);
            }
        }
        for t in &obj.triangles {
            for c in t {
                if skin.get(c.v as usize).copied().flatten().is_none() {
                    let n = c.v + 1;
                    let line = obj.position_lines.get(c.v as usize).copied().unwrap_or(0);
                    return Err(root.err(
                        mesh_line,
                        &format!("OBJ vertex {n} (`{obj_path}` line {line}) has no `[weights.{n}]` table"),
                    ));
                }
            }
        }
        let skin: Vec<SkinInfluence> = skin.into_iter().map(Option::unwrap_or_default).collect();
        let mesh = build(obj_path, &obj, opts, Some(&skin))?;
        Ok(cooked(
            output_name(source.path, ".skinmesh.toml", ".skinmesh"),
            &mesh,
        ))
    }
}

/// One `[weights.<v>]` table: the 0-based position index and the quantized influence.
fn weights_table(
    rest: &str,
    table: &Fields<'_>,
    positions: usize,
) -> Result<(usize, SkinInfluence), CookError> {
    table.only(&["joints", "weights"])?;
    let index = rest
        .parse::<usize>()
        .ok()
        .filter(|v| (1..=positions).contains(v))
        .ok_or_else(|| {
            table.err(
                table.line(),
                &format!("`{rest}` is not an OBJ vertex number (1 to {positions})"),
            )
        })?;
    let joints = table.ints("joints")?;
    let weights = table.floats("weights")?;
    let joints_line = table.line_of("joints");
    let weights_line = table.line_of("weights");
    if joints.is_empty() || joints.len() > 4 {
        return Err(table.err(joints_line, "a vertex has 1 to 4 joints"));
    }
    if weights.len() != joints.len() {
        return Err(table.err(
            weights_line,
            &format!("{} weights for {} joints", weights.len(), joints.len()),
        ));
    }
    let mut out = SkinInfluence::default();
    for (i, (slot, j)) in out.joints.iter_mut().zip(&joints).enumerate() {
        *slot = u16::try_from(*j)
            .map_err(|_| table.err(joints_line, &format!("joint `{j}` is not a bone index")))?;
        if joints.get(..i).is_some_and(|before| before.contains(j)) {
            return Err(table.err(joints_line, &format!("joint {j} repeats")));
        }
    }
    if weights.iter().any(|w| *w < 0.0) {
        return Err(table.err(weights_line, "weights must not be negative"));
    }
    let sum: f32 = weights.iter().sum();
    if (sum - 1.0).abs() > WEIGHT_SUM_TOLERANCE {
        return Err(table.err(weights_line, &format!("weights sum to {sum}, not 1")));
    }
    out.weights = quantize_weights(&weights);
    Ok((index - 1, out))
}
