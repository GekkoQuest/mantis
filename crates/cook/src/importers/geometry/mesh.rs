//! OBJ to MMSH: handedness conversion, normals, welding, meshlets.
//!
//! Handedness (decision 0019): OBJ is right-handed with +X right, +Y up, +Z toward the
//! viewer, and wound counter-clockwise seen from outside, so `(b - a) x (c - a)` points
//! outward. The cook negates Z of every position and normal (a mirror, determinant -1),
//! which turns that cross product inward, so it also swaps the second and third corner of
//! every triangle: `(a, b, c)` becomes `(a, c, b)` and the cross product points outward
//! again, as MMSH requires. Texture V is flipped (`v' = 1 - v`) because OBJ puts the V
//! origin at the bottom of the image and the runtime at the top.

use std::collections::HashMap;

use mantis_formats::mesh::{MAX_INDICES, MAX_VERTICES, MeshAsset, MeshVertex, Meshlet, SkinInfluence};

use super::meshlets::{bounding_sphere, cluster, face_normal, normal_cone, normalize};
use super::obj::{Corner, Obj};
use crate::importer::CookError;

/// Import settings.
#[derive(Clone, Copy, Debug)]
pub(crate) struct MeshOptions {
    /// Uniform scale applied to positions.
    pub(crate) scale: f32,
    /// Take uv1 from the `vt2` set when there is one.
    pub(crate) lightmap_uv: bool,
}

type WeldKey = ([u32; 10], [u16; 4], [u8; 4]);

/// `-0` as `+0`, so mirrored zeros read the same everywhere.
fn positive_zero(x: f32) -> f32 {
    if x == 0.0 { 0.0 } else { x }
}

fn bits<const N: usize>(v: [f32; N]) -> [u32; N] {
    // `+ 0.0` folds -0 into +0 so they weld.
    v.map(|c| (c + 0.0).to_bits())
}

fn at<T: Copy + Default>(v: &[T], i: u32) -> T {
    v.get(i as usize).copied().unwrap_or_default()
}

/// Area-weighted vertex normals per position (up when no face gives a direction).
fn smooth_normals(positions: &[[f32; 3]], tris: &[[Corner; 3]]) -> Vec<[f32; 3]> {
    let mut smooth = vec![[0.0f32; 3]; positions.len()];
    for t in tris {
        let f = face_normal(t.map(|c| at(positions, c.v)));
        for c in t {
            if let Some(acc) = smooth.get_mut(c.v as usize) {
                *acc = [acc[0] + f[0], acc[1] + f[1], acc[2] + f[2]];
            }
        }
    }
    smooth
        .into_iter()
        .map(|n| normalize(n).unwrap_or([0.0, 1.0, 0.0]))
        .collect()
}

/// The `vt2` set when lightmap UVs are wanted and present.
fn lightmap_set<'o>(
    path: &str,
    obj: &'o Obj,
    options: MeshOptions,
) -> Result<Option<&'o [[f32; 2]]>, CookError> {
    if !options.lightmap_uv || obj.uvs2.is_empty() {
        return Ok(None);
    }
    if obj.uvs2.len() != obj.uvs.len() {
        return Err(CookError::at(
            path,
            obj.uv2_line,
            &format!(
                "{} `vt2` lines for {} `vt` lines (the sets are parallel)",
                obj.uvs2.len(),
                obj.uvs.len()
            ),
        ));
    }
    Ok(Some(&obj.uvs2))
}

/// Converts `obj` to a validated mesh. `skin`, when given, holds the influences of each
/// OBJ position (every position a face uses must have one).
pub(crate) fn build(
    path: &str,
    obj: &Obj,
    options: MeshOptions,
    skin: Option<&[SkinInfluence]>,
) -> Result<MeshAsset, CookError> {
    let s = options.scale;
    let positions: Vec<[f32; 3]> = obj
        .positions
        .iter()
        .map(|p| [p[0] * s, p[1] * s, -p[2] * s].map(positive_zero))
        .collect();
    if let Some(i) = positions.iter().position(|p| p.iter().any(|c| !c.is_finite())) {
        let line = obj.position_lines.get(i).copied().unwrap_or(0);
        return Err(CookError::at(path, line, "the scaled position is not finite"));
    }
    let normals: Vec<[f32; 3]> = obj
        .normals
        .iter()
        .map(|n| [n[0], n[1], -n[2]].map(positive_zero))
        .collect();
    let uv2 = lightmap_set(path, obj, options)?;
    // Mirrored, so swap two corners to keep the winding outward.
    let tris: Vec<_> = obj.triangles.iter().map(|[a, b, c]| [*a, *c, *b]).collect();
    let smooth = smooth_normals(&positions, &tris);

    let mut weld: HashMap<WeldKey, u32> = HashMap::new();
    let mut position_ids: HashMap<[u32; 3], u32> = HashMap::new();
    let mut vertices: Vec<MeshVertex> = Vec::new();
    let mut influences: Vec<SkinInfluence> = Vec::new();
    let mut vertex_position: Vec<u32> = Vec::new();
    let mut triangles: Vec<[u32; 3]> = Vec::new();
    let mut adjacency: Vec<[u32; 3]> = Vec::new();
    for t in &tris {
        let mut tri = [0u32; 3];
        let mut adj = [0u32; 3];
        for ((slot, pid), c) in tri.iter_mut().zip(adj.iter_mut()).zip(t) {
            let position = at(&positions, c.v);
            let normal = c.vn.map_or_else(|| at(&smooth, c.v), |n| at(&normals, n));
            let uv0 = c.vt.map_or([0.0; 2], |i| {
                let uv: [f32; 2] = at(&obj.uvs, i);
                [uv[0], 1.0 - uv[1]]
            });
            let uv1 = match (uv2, c.vt) {
                (Some(set), Some(i)) => {
                    let uv: [f32; 2] = at(set, i);
                    [uv[0], 1.0 - uv[1]]
                }
                _ => [0.0; 2],
            };
            let influence = skin.map(|sk| at(sk, c.v)).unwrap_or_default();
            let vertex = MeshVertex {
                position,
                normal,
                uv0,
                uv1,
            };
            let [p0, p1, p2] = bits(position);
            let [n0, n1, n2] = bits(normal);
            let [u0, u1] = bits(uv0);
            let [w0, w1] = bits(uv1);
            let key = (
                [p0, p1, p2, n0, n1, n2, u0, u1, w0, w1],
                influence.joints,
                influence.weights,
            );
            let next_position = u32::try_from(position_ids.len()).unwrap_or(u32::MAX);
            let position_id = *position_ids.entry([p0, p1, p2]).or_insert(next_position);
            let next = u32::try_from(vertices.len()).unwrap_or(u32::MAX);
            *slot = *weld.entry(key).or_insert_with(|| {
                vertices.push(vertex);
                influences.push(influence);
                vertex_position.push(position_id);
                next
            });
            *pid = position_id;
        }
        // A triangle with a repeated position has no area: drop it.
        if adj[0] != adj[1] && adj[1] != adj[2] && adj[0] != adj[2] {
            triangles.push(tri);
            adjacency.push(adj);
        }
    }
    if triangles.is_empty() {
        return Err(CookError::at(path, 0, "every face is degenerate"));
    }
    if vertices.len() > MAX_VERTICES as usize || triangles.len() * 3 > MAX_INDICES as usize {
        return Err(CookError::at(
            path,
            0,
            &format!(
                "{} vertices and {} indices exceed the mesh limits ({MAX_VERTICES}, {MAX_INDICES})",
                vertices.len(),
                triangles.len() * 3
            ),
        ));
    }
    let centroids: Vec<[f32; 3]> = triangles.iter().map(|t| centroid(&vertices, *t)).collect();
    let clusters = cluster(&triangles, &adjacency, &centroids, position_ids.len());
    assemble(
        path,
        &vertices,
        skin.map(|_| influences.as_slice()),
        &triangles,
        &clusters,
    )
}

fn centroid(vertices: &[MeshVertex], t: [u32; 3]) -> [f32; 3] {
    let [a, b, c] = t.map(|v| vertices.get(v as usize).map(|x| x.position).unwrap_or_default());
    [
        (a[0] + b[0] + c[0]) / 3.0,
        (a[1] + b[1] + c[1]) / 3.0,
        (a[2] + b[2] + c[2]) / 3.0,
    ]
}

/// Orders vertices by first use, writes meshlet-ordered indices and meshlet bounds, and
/// validates the result with the runtime rules.
fn assemble(
    path: &str,
    vertices: &[MeshVertex],
    skin: Option<&[SkinInfluence]>,
    triangles: &[[u32; 3]],
    clusters: &[Vec<usize>],
) -> Result<MeshAsset, CookError> {
    let mut remap = vec![u32::MAX; vertices.len()];
    let mut out_vertices = Vec::with_capacity(vertices.len());
    let mut out_skin = Vec::new();
    let mut indices = Vec::with_capacity(triangles.len() * 3);
    let mut meshlets = Vec::with_capacity(clusters.len());
    for c in clusters {
        let first_index = u32::try_from(indices.len()).unwrap_or(u32::MAX);
        let mut points = Vec::new();
        let mut normals = Vec::new();
        for t in c {
            let tri = triangles.get(*t).copied().unwrap_or_default();
            for v in tri {
                let Some(slot) = remap.get_mut(v as usize) else {
                    continue;
                };
                if *slot == u32::MAX {
                    *slot = u32::try_from(out_vertices.len()).unwrap_or(u32::MAX);
                    out_vertices.push(vertices.get(v as usize).copied().unwrap_or_default());
                    if let Some(sk) = skin {
                        out_skin.push(sk.get(v as usize).copied().unwrap_or_default());
                    }
                }
                indices.push(*slot);
                points.push(vertices.get(v as usize).map(|x| x.position).unwrap_or_default());
            }
            let p = tri.map(|v| vertices.get(v as usize).map(|x| x.position).unwrap_or_default());
            if let Some(n) = normalize(face_normal(p)) {
                normals.push(n);
            }
        }
        let (center, radius) = bounding_sphere(&points);
        let (cone_axis, cone_cutoff) = normal_cone(&normals);
        meshlets.push(Meshlet {
            first_index,
            index_count: u32::try_from(indices.len()).unwrap_or(u32::MAX) - first_index,
            center,
            radius,
            cone_axis,
            cone_cutoff,
        });
    }
    let mut bounds_min = [f32::INFINITY; 3];
    let mut bounds_max = [f32::NEG_INFINITY; 3];
    for v in &out_vertices {
        for ((lo, hi), p) in bounds_min.iter_mut().zip(bounds_max.iter_mut()).zip(v.position) {
            *lo = lo.min(p);
            *hi = hi.max(p);
        }
    }
    let mesh = MeshAsset {
        vertices: out_vertices,
        skin: skin.map(|_| out_skin),
        indices,
        meshlets,
        bounds_min,
        bounds_max,
    };
    MeshAsset::parse(&mesh.encode())
        .map_err(|e| CookError::at(path, 0, &format!("the cooked mesh does not load: {e}")))
}

/// Quantizes non-negative weights (summing to about 1) to bytes that sum to exactly 255,
/// by largest remainder (ties to the lower slot).
#[expect(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // Floors of values in 0..=255.
pub(crate) fn quantize_weights(weights: &[f32]) -> [u8; 4] {
    let total: f64 = weights.iter().map(|w| f64::from(*w)).sum();
    let mut out = [0u8; 4];
    if total <= 0.0 {
        return out;
    }
    let mut fractions = Vec::with_capacity(4);
    let mut assigned = 0u32;
    for (i, (slot, w)) in out.iter_mut().zip(weights).enumerate() {
        let scaled = f64::from(*w) / total * 255.0;
        let floor = scaled.floor().clamp(0.0, 255.0);
        *slot = floor as u8;
        assigned += u32::from(*slot);
        fractions.push((scaled - floor, i));
    }
    fractions.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(&b.1)));
    let mut left = 255u32.saturating_sub(assigned);
    for (_, i) in fractions.iter().cycle() {
        if left == 0 {
            break;
        }
        if let Some(slot) = out.get_mut(*i)
            && *slot < 255
        {
            *slot += 1;
            left -= 1;
        }
    }
    out
}
