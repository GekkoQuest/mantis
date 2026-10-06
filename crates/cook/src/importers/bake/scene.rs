//! The bake scene: every occluding triangle of a sector in world space, and the
//! lightmapped placements' triangles with their lightmap coordinates.
//!
//! Occluders are the ground heightfield (two triangles per cell, facing up), the box hulls
//! that block sight (twelve triangles each, facing out), and the meshes of placements that
//! cast shadows (transformed by the placement's similarity transform).

use std::collections::BTreeMap;

use mantis_formats::bundle::AssetKind;
use mantis_formats::mesh::MeshAsset;
use mantis_formats::sector::HULL_BLOCKS_SIGHT;

use super::bvh::{Bvh, Triangle};
use super::math::V3;
use crate::importer::{CookError, ImportContext};
use crate::importers::world::{PlacementSource, SectorSource};

/// One triangle of a lightmapped placement.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ReceiverTriangle {
    /// World positions.
    pub positions: [V3; 3],
    /// World unit normals (interpolated for shading).
    pub normals: [V3; 3],
    /// Lightmap coordinates (mesh `uv1`, 0 to 1).
    pub uv: [[f32; 2]; 3],
    /// Unit front-face normal (ray origins are offset along it).
    pub face: V3,
}

/// A lightmapped placement.
#[derive(Clone, Debug, PartialEq)]
pub struct Receiver {
    /// Placement name.
    pub name: String,
    /// Source line of the placement (for errors).
    pub line: usize,
    /// World surface area (square meters).
    pub area: f32,
    /// Non-degenerate triangles, in mesh order.
    pub triangles: Vec<ReceiverTriangle>,
}

/// Everything the bakers trace against and light.
#[derive(Debug, Default)]
pub struct Scene {
    /// Every occluding triangle.
    pub bvh: Bvh,
    /// Lightmapped placements, in source order.
    pub receivers: Vec<Receiver>,
    /// Lowest and highest ground sample, when the sector has ground.
    pub ground_range: Option<(f32, f32)>,
}

fn oriented(a: V3, b: V3, c: V3, outward: V3) -> Triangle {
    let t = Triangle { a, b, c };
    if t.normal().dot(outward) < 0.0 {
        Triangle { a, b: c, c: b }
    } else {
        t
    }
}

fn push_quad(out: &mut Vec<Triangle>, quad: [V3; 4], outward: V3) {
    let [c0, c1, c2, c3] = quad;
    out.push(oriented(c0, c1, c2, outward));
    out.push(oriented(c0, c2, c3, outward));
}

/// The twelve outward-facing triangles of an axis-aligned box.
pub fn box_triangles(min: [f32; 3], max: [f32; 3]) -> Vec<Triangle> {
    let [x0, y0, z0] = min;
    let [x1, y1, z1] = max;
    let p = V3::new;
    let mut out = Vec::with_capacity(12);
    push_quad(
        &mut out,
        [p(x1, y0, z0), p(x1, y1, z0), p(x1, y1, z1), p(x1, y0, z1)],
        p(1.0, 0.0, 0.0),
    );
    push_quad(
        &mut out,
        [p(x0, y0, z0), p(x0, y0, z1), p(x0, y1, z1), p(x0, y1, z0)],
        p(-1.0, 0.0, 0.0),
    );
    push_quad(
        &mut out,
        [p(x0, y1, z0), p(x0, y1, z1), p(x1, y1, z1), p(x1, y1, z0)],
        p(0.0, 1.0, 0.0),
    );
    push_quad(
        &mut out,
        [p(x0, y0, z0), p(x1, y0, z0), p(x1, y0, z1), p(x0, y0, z1)],
        p(0.0, -1.0, 0.0),
    );
    push_quad(
        &mut out,
        [p(x0, y0, z1), p(x1, y0, z1), p(x1, y1, z1), p(x0, y1, z1)],
        p(0.0, 0.0, 1.0),
    );
    push_quad(
        &mut out,
        [p(x0, y0, z0), p(x0, y1, z0), p(x1, y1, z0), p(x1, y0, z0)],
        p(0.0, 0.0, -1.0),
    );
    out
}

#[allow(clippy::cast_precision_loss)] // Grid indices are at most 4097.
fn ground_triangles(sector: &SectorSource, out: &mut Vec<Triangle>) -> Option<(f32, f32)> {
    let g = sector.ground.as_ref()?;
    let (w, d) = (g.width as usize, g.depth as usize);
    let at = |i: usize, j: usize| {
        let h = g.heights.get(j * w + i).copied().unwrap_or(0.0);
        V3::new(
            g.origin_x + i as f32 * g.cell_size,
            h,
            g.origin_z + j as f32 * g.cell_size,
        )
    };
    let up = V3::new(0.0, 1.0, 0.0);
    for j in 0..d.saturating_sub(1) {
        for i in 0..w.saturating_sub(1) {
            push_quad(out, [at(i, j), at(i, j + 1), at(i + 1, j + 1), at(i + 1, j)], up);
        }
    }
    let lo = g.heights.iter().copied().fold(f32::MAX, f32::min);
    let hi = g.heights.iter().copied().fold(f32::MIN, f32::max);
    (lo <= hi).then_some((lo, hi))
}

/// A placement's world transform applied to a point and to a normal.
fn transform(p: &PlacementSource) -> (impl Fn([f32; 3]) -> V3, impl Fn([f32; 3]) -> V3) {
    let m = p.transform;
    let axis = |k: usize| {
        V3::new(
            m.get(k * 3).copied().unwrap_or(0.0),
            m.get(k * 3 + 1).copied().unwrap_or(0.0),
            m.get(k * 3 + 2).copied().unwrap_or(0.0),
        )
    };
    let (ax, ay, az, t) = (axis(0), axis(1), axis(2), axis(3));
    (
        move |[x, y, z]: [f32; 3]| ax * x + ay * y + az * z + t,
        move |[x, y, z]: [f32; 3]| (ax * x + ay * y + az * z).normalized(),
    )
}

fn mesh_of<'m>(
    p: &PlacementSource,
    ctx: &ImportContext<'_>,
    path: &str,
    cache: &'m mut BTreeMap<String, MeshAsset>,
) -> Result<&'m MeshAsset, CookError> {
    if !cache.contains_key(&p.mesh) {
        let (_, bytes) = ctx.resolve_bytes(&p.mesh, AssetKind::Mesh, path, p.line)?;
        let mesh = MeshAsset::parse(bytes)
            .map_err(|e| CookError::at(path, p.line, &format!("mesh `{}` does not parse: {e}", p.mesh)))?;
        cache.insert(p.mesh.clone(), mesh);
    }
    cache
        .get(&p.mesh)
        .ok_or_else(|| CookError::at(path, p.line, &format!("mesh `{}` is missing", p.mesh)))
}

fn receiver(p: &PlacementSource, mesh: &MeshAsset, path: &str) -> Result<Receiver, CookError> {
    if let Some((index, vertex)) = mesh
        .vertices
        .iter()
        .enumerate()
        .find(|(_, vertex)| vertex.uv1.iter().any(|c| !(0.0..=1.0).contains(c)))
    {
        return Err(CookError::at(
            path,
            p.line,
            &format!(
                "placement `{}` is lightmapped but mesh `{}` has lightmap coordinates (uv1) outside 0..1 (vertex {index}: {:?})",
                p.name, p.mesh, vertex.uv1
            ),
        ));
    }
    let (point, normal) = transform(p);
    let mut triangles = Vec::with_capacity(mesh.indices.len() / 3);
    let mut area = 0.0f32;
    let (corners, _) = mesh.indices.as_chunks::<3>();
    for tri in corners {
        let vs: Vec<_> = tri
            .iter()
            .filter_map(|i| mesh.vertices.get(*i as usize))
            .collect();
        let [va, vb, vc] = <[_; 3]>::try_from(vs.as_slice()).map_err(|_| {
            CookError::at(
                path,
                p.line,
                &format!("mesh `{}` has an index out of range", p.mesh),
            )
        })?;
        let positions = [point(va.position), point(vb.position), point(vc.position)];
        let [pa, pb, pc] = positions;
        let world = Triangle { a: pa, b: pb, c: pc };
        let double = world.double_area();
        if double <= 1e-12 {
            continue;
        }
        area += double * 0.5;
        triangles.push(ReceiverTriangle {
            positions,
            normals: [normal(va.normal), normal(vb.normal), normal(vc.normal)],
            uv: [va.uv1, vb.uv1, vc.uv1],
            face: world.normal(),
        });
    }
    if triangles.is_empty() {
        return Err(CookError::at(
            path,
            p.line,
            &format!(
                "placement `{}` is lightmapped but its mesh has no surface",
                p.name
            ),
        ));
    }
    Ok(Receiver {
        name: p.name.clone(),
        line: p.line,
        area,
        triangles,
    })
}

/// Builds the scene of `sector` (meshes resolved through `ctx`; errors at `path`).
///
/// # Errors
/// [`CookError`] at a placement whose mesh is missing or does not parse, or whose
/// lightmap coordinates leave 0..1.
pub fn build(sector: &SectorSource, ctx: &ImportContext<'_>, path: &str) -> Result<Scene, CookError> {
    let mut occluders = Vec::new();
    let ground_range = ground_triangles(sector, &mut occluders);
    for h in &sector.hulls {
        if h.flags & HULL_BLOCKS_SIGHT != 0 {
            occluders.extend(box_triangles(h.aabb_min, h.aabb_max));
        }
    }
    let mut cache = BTreeMap::new();
    let mut receivers = Vec::new();
    for p in &sector.placements {
        if !p.casts_shadows && !p.lightmapped {
            continue;
        }
        let mesh = mesh_of(p, ctx, path, &mut cache)?;
        if p.casts_shadows {
            let (point, _) = transform(p);
            let (corners, _) = mesh.indices.as_chunks::<3>();
            for tri in corners {
                let mut corners = tri
                    .iter()
                    .filter_map(|i| mesh.vertices.get(*i as usize).map(|v| point(v.position)));
                if let (Some(a), Some(b), Some(c)) = (corners.next(), corners.next(), corners.next()) {
                    let t = Triangle { a, b, c };
                    if t.double_area() > 1e-12 {
                        occluders.push(t);
                    }
                }
            }
        }
        if p.lightmapped {
            receivers.push(receiver(p, mesh, path)?);
        }
    }
    Ok(Scene {
        bvh: Bvh::build(&occluders),
        receivers,
        ground_range,
    })
}
