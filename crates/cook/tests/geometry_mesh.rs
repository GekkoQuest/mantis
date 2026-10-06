//! Mesh importers: OBJ parsing, handedness, normals, welding, meshlets, sidecars, skinned
//! meshes, errors with locations, determinism.

#![expect(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::indexing_slicing
)]

use std::fmt::Write as _;

use mantis_cook::tree::ContentTree;
use mantis_cook::{Cook, CookOutput, importers};
use mantis_formats::bundle::{AssetKind, Domain};
use mantis_formats::mesh::{MESHLET_MAX_TRIANGLES, MESHLET_MAX_VERTICES, MeshAsset};

type TestResult = Result<(), Box<dyn std::error::Error>>;

fn run(tree: &ContentTree) -> Result<CookOutput, String> {
    let cook = Cook::new(importers::builtin()).map_err(|e| e.to_string())?;
    cook.run(tree).map_err(|e| format!("{e:?}"))
}

fn errors(tree: &ContentTree) -> Result<Vec<String>, String> {
    let cook = Cook::new(importers::builtin()).map_err(|e| e.to_string())?;
    match cook.run(tree) {
        Ok(_) => Err("expected the cook to fail".into()),
        Err(e) => Ok(e.iter().map(ToString::to_string).collect()),
    }
}

fn cook_one(path: &str, text: &str) -> Result<MeshAsset, Box<dyn std::error::Error>> {
    let mut t = ContentTree::new();
    t.insert(path, text);
    let out = run(&t)?;
    let name = format!("{}.mesh", path.strip_suffix(".obj").ok_or("not an obj")?);
    let asset = out.get(&name).ok_or("no mesh output")?;
    assert_eq!(asset.kind, AssetKind::Mesh);
    assert_eq!(asset.domain, Domain::Presentation);
    Ok(MeshAsset::parse(&asset.bytes)?)
}

fn sub(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}

fn cross(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}

fn dot(a: [f32; 3], b: [f32; 3]) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

fn triangles(mesh: &MeshAsset) -> Vec<[[f32; 3]; 3]> {
    mesh.indices
        .as_chunks::<3>()
        .0
        .iter()
        .map(|t| t.map(|i| mesh.vertices[i as usize].position))
        .collect()
}

/// An axis-aligned cube from -1 to 1, faces wound counter-clockwise from outside
/// (OBJ's right-handed convention), optionally with per-face normals.
fn cube_obj(face_normals: bool) -> String {
    let mut s = String::from(
        "# cube\no cube\nv -1 -1 -1\nv 1 -1 -1\nv 1 1 -1\nv -1 1 -1\nv -1 -1 1\nv 1 -1 1\nv 1 1 1\nv -1 1 1\n",
    );
    let faces = [
        ([5, 6, 7, 8], [0, 0, 1]),
        ([2, 1, 4, 3], [0, 0, -1]),
        ([2, 3, 7, 6], [1, 0, 0]),
        ([1, 5, 8, 4], [-1, 0, 0]),
        ([4, 8, 7, 3], [0, 1, 0]),
        ([1, 2, 6, 5], [0, -1, 0]),
    ];
    if face_normals {
        for (_, n) in faces {
            let _ = writeln!(s, "vn {} {} {}", n[0], n[1], n[2]);
        }
    }
    s.push_str("s 1\nusemtl crate\n");
    for (k, (f, _)) in faces.iter().enumerate() {
        s.push('f');
        for v in f {
            if face_normals {
                let _ = write!(s, " {v}//{}", k + 1);
            } else {
                let _ = write!(s, " {v}");
            }
        }
        s.push('\n');
    }
    s
}

/// A flat grid of `n` by `n` quads in the XY plane, facing +Z (OBJ frame).
fn grid_obj(n: usize) -> String {
    let mut s = String::new();
    for y in 0..=n {
        for x in 0..=n {
            let _ = writeln!(s, "v {x} {y} 0");
        }
    }
    let id = |x: usize, y: usize| y * (n + 1) + x + 1;
    for y in 0..n {
        for x in 0..n {
            let _ = writeln!(
                s,
                "f {} {} {} {}",
                id(x, y),
                id(x + 1, y),
                id(x + 1, y + 1),
                id(x, y + 1)
            );
        }
    }
    s
}

/// A UV sphere of radius 2, every triangle wound outward (counter-clockwise from outside).
fn sphere_obj(rings: usize, segments: usize) -> String {
    let mut points: Vec<[f32; 3]> = vec![[0.0, 2.0, 0.0]];
    for i in 1..rings {
        let theta = std::f32::consts::PI * i as f32 / rings as f32;
        for j in 0..segments {
            let phi = std::f32::consts::TAU * j as f32 / segments as f32;
            points.push([
                2.0 * theta.sin() * phi.cos(),
                2.0 * theta.cos(),
                2.0 * theta.sin() * phi.sin(),
            ]);
        }
    }
    points.push([0.0, -2.0, 0.0]);
    let bottom = points.len() - 1;
    let ring = |i: usize, j: usize| 1 + (i - 1) * segments + (j % segments);
    let mut tris: Vec<[usize; 3]> = Vec::new();
    for j in 0..segments {
        tris.push([0, ring(1, j), ring(1, j + 1)]);
        tris.push([bottom, ring(rings - 1, j + 1), ring(rings - 1, j)]);
        for i in 1..rings - 1 {
            tris.push([ring(i, j), ring(i + 1, j), ring(i + 1, j + 1)]);
            tris.push([ring(i, j), ring(i + 1, j + 1), ring(i, j + 1)]);
        }
    }
    let mut out = String::new();
    for v in &points {
        let _ = writeln!(out, "v {} {} {}", v[0], v[1], v[2]);
    }
    for t in &mut tris {
        let [a, b, c] = t.map(|i| points[i]);
        let center = [
            (a[0] + b[0] + c[0]) / 3.0,
            (a[1] + b[1] + c[1]) / 3.0,
            (a[2] + b[2] + c[2]) / 3.0,
        ];
        if dot(cross(sub(b, a), sub(c, a)), center) < 0.0 {
            t.swap(1, 2);
        }
        let _ = writeln!(out, "f {} {} {}", t[0] + 1, t[1] + 1, t[2] + 1);
    }
    out
}

#[test]
fn every_index_form_and_negative_indices_cook_the_same_quad() -> TestResult {
    let header = "v 0 0 0\nv 1 0 0\nv 1 1 0\nv 0 1 0\nvt 0 0\nvt 1 0\nvt 1 1\nvt 0 1\nvn 0 0 1\n";
    let forms = [
        "f 1/1/1 2/2/1 3/3/1 4/4/1\n",
        "f -4/-4/-1 -3/-3/-1 -2/-2/-1 -1/-1/-1\n",
        "f 1/1/1 -3/2/1 3/-2/-1 4/4/1\n",
    ];
    let mut cooked = Vec::new();
    for f in forms {
        cooked.push(cook_one("meshes/quad.obj", &format!("{header}{f}"))?);
    }
    assert!(cooked.windows(2).all(|w| w[0] == w[1]));
    let quad = &cooked[0];
    assert_eq!(quad.indices.len(), 6);
    assert_eq!(quad.vertices.len(), 4);
    // V is flipped: OBJ (1, 1) becomes (1, 0).
    assert!(
        quad.vertices
            .iter()
            .any(|v| v.uv0 == [1.0, 0.0] && v.position == [1.0, 1.0, 0.0])
    );
    // Position-only and position//normal forms.
    let plain = cook_one("meshes/a.obj", "v 0 0 0\nv 1 0 0\nv 0 1 0\nf 1 2 3\n")?;
    let with_normal = cook_one(
        "meshes/b.obj",
        "v 0 0 0\nv 1 0 0\nv 0 1 0\nvn 0 0 1\nf 1//1 2//1 3//1\n",
    )?;
    assert_eq!(plain.vertices.len(), 3);
    assert_eq!(
        plain, with_normal,
        "the smooth normal of one face is its face normal"
    );
    let uv_only = cook_one(
        "meshes/c.obj",
        "v 0 0 0\nv 1 0 0\nv 0 1 0\nvt 0.5 0.25\nf 1/1 2/1 3/1\n",
    )?;
    assert!(uv_only.vertices.iter().all(|v| v.uv0 == [0.5, 0.75]));
    Ok(())
}

#[test]
fn polygons_are_fan_triangulated() -> TestResult {
    let mut obj = String::new();
    for k in 0..6 {
        let a = std::f32::consts::TAU * k as f32 / 6.0;
        let _ = writeln!(obj, "v {} {} 0", a.cos(), a.sin());
    }
    obj.push_str("f 1 2 3 4 5\nf 1 2 3 4 5 6\n");
    let mesh = cook_one("meshes/poly.obj", &obj)?;
    assert_eq!(mesh.indices.len() / 3, 3 + 4);
    // Every triangle faces -Z after conversion (the polygon faces +Z in OBJ).
    for t in triangles(&mesh) {
        assert!(cross(sub(t[1], t[0]), sub(t[2], t[0]))[2] < 0.0);
    }
    Ok(())
}

#[test]
fn missing_normals_are_smoothed_by_area() -> TestResult {
    // A flat quad: every computed normal is the face normal, -Z after conversion.
    let flat = cook_one(
        "meshes/flat.obj",
        "v 0 0 0\nv 2 0 0\nv 2 2 0\nv 0 2 0\nf 1 2 3 4\n",
    )?;
    assert!(flat.vertices.iter().all(|v| v.normal == [0.0, 0.0, -1.0]));
    // A smooth cube: each corner normal points along the corner's diagonal.
    let cube = cook_one("meshes/cube.obj", &cube_obj(false))?;
    assert_eq!(cube.vertices.len(), 8, "one vertex per position");
    // Triangle-area weighting leans toward the faces whose both triangles touch the
    // corner, so the normal is near (not exactly on) the diagonal.
    let k = 1.0 / 3.0f32.sqrt();
    for v in &cube.vertices {
        let expect = v.position.map(|c| c * k);
        assert!(dot(v.normal, expect) > 0.9, "{v:?}");
        assert!(
            (0..3).all(|i| v.normal[i].signum() == v.position[i].signum()),
            "{v:?}"
        );
    }
    // Area weighting: a large and a small face meeting at a vertex.
    let mesh = cook_one(
        "meshes/fold.obj",
        "v 0 0 0\nv 10 0 0\nv 0 10 0\nv 0 0 1\nf 1 2 3\nf 1 4 2\n",
    )?;
    let origin = mesh
        .vertices
        .iter()
        .find(|v| v.position == [0.0; 3])
        .ok_or("origin")?;
    // The big face (normal -Z after conversion) outweighs the small one (normal +Y):
    // the sum is (0, 10, -100) normalized.
    assert!(
        origin.normal[2] < -0.99 && origin.normal[1] > 0.09,
        "{:?}",
        origin.normal
    );
    Ok(())
}

#[test]
fn handedness_conversion_keeps_outward_triangles_outward() -> TestResult {
    for face_normals in [false, true] {
        let mesh = cook_one("meshes/cube.obj", &cube_obj(face_normals))?;
        assert_eq!(mesh.indices.len(), 36);
        for chunk in mesh.indices.as_chunks::<3>().0 {
            let [a, b, c] = chunk.map(|i| mesh.vertices[i as usize]);
            let n = cross(sub(b.position, a.position), sub(c.position, a.position));
            let centroid = [
                a.position[0] + b.position[0] + c.position[0],
                a.position[1] + b.position[1] + c.position[1],
                a.position[2] + b.position[2] + c.position[2],
            ];
            assert!(dot(n, centroid) > 0.0, "inward triangle {chunk:?}");
            for v in [a, b, c] {
                assert!(dot(v.normal, n) > 0.0, "normal disagrees with winding");
            }
        }
    }
    // Z is the mirrored axis: an OBJ point at z = +1 (toward the viewer) cooks to z = -1.
    let mesh = cook_one(
        "meshes/tri.obj",
        "v 0 0 1\nv 1 0 1\nv 0 1 1\nvn 0 0 1\nf 1//1 2//1 3//1\n",
    )?;
    assert!(
        mesh.vertices
            .iter()
            .all(|v| v.position[2] == -1.0 && v.normal == [0.0, 0.0, -1.0])
    );
    let t = triangles(&mesh)[0];
    assert!(
        cross(sub(t[1], t[0]), sub(t[2], t[0]))[2] < 0.0,
        "still faces its normal"
    );
    Ok(())
}

#[test]
fn identical_vertices_weld() -> TestResult {
    let hard = cook_one("meshes/cube.obj", &cube_obj(true))?;
    assert_eq!(hard.vertices.len(), 24, "per-face normals split corners");
    // Repeated `v` lines with the same coordinates weld; -0 welds with 0.
    let dup = cook_one(
        "meshes/dup.obj",
        "v 0 0 0\nv 1 0 0\nv 1 1 0\nv -0 0 0\nv 1 1 0\nv 0 1 0\nf 1 2 3\nf 4 5 6\n",
    )?;
    assert_eq!(dup.vertices.len(), 4);
    assert_eq!(dup.indices.len(), 6);
    // A face that collapses onto a repeated position is dropped.
    let degenerate = cook_one("meshes/deg.obj", "v 0 0 0\nv 1 0 0\nv 0 1 0\nf 1 2 3\nf 1 1 2\n")?;
    assert_eq!(degenerate.indices.len(), 3);
    Ok(())
}

#[test]
fn meshlets_respect_limits_and_cover_every_triangle() -> TestResult {
    for (name, obj) in [("grid", grid_obj(40)), ("sphere", sphere_obj(24, 48))] {
        let mesh = cook_one(&format!("meshes/{name}.obj"), &obj)?;
        let total = mesh.indices.len() / 3;
        let mut covered = 0;
        for m in &mesh.meshlets {
            assert_eq!(m.first_index as usize, covered * 3);
            let tris = m.index_count as usize / 3;
            assert!(tris >= 1 && tris <= MESHLET_MAX_TRIANGLES as usize);
            let range = m.first_index as usize..(m.first_index + m.index_count) as usize;
            let mut distinct: Vec<u32> = mesh.indices[range].to_vec();
            distinct.sort_unstable();
            distinct.dedup();
            assert!(
                distinct.len() <= MESHLET_MAX_VERTICES,
                "{} vertices",
                distinct.len()
            );
            covered += tris;
        }
        assert_eq!(covered, total);
        // Spatially coherent and well filled: close to the fewest meshlets possible.
        let fewest = total.div_ceil(MESHLET_MAX_TRIANGLES as usize);
        assert!(
            mesh.meshlets.len() <= fewest * 2,
            "{name}: {} meshlets for {total} triangles",
            mesh.meshlets.len()
        );
    }
    // The grid's 3200 triangles all survive; the source's triangles are all present.
    let grid = cook_one("meshes/grid.obj", &grid_obj(40))?;
    assert_eq!(grid.indices.len() / 3, 3200);
    let mut seen: Vec<[u32; 3]> = grid
        .indices
        .as_chunks::<3>()
        .0
        .iter()
        .map(|t| {
            let mut k = t.map(|i| {
                let p = grid.vertices[i as usize].position;
                (p[0] as u32) * 1000 + p[1] as u32
            });
            k.sort_unstable();
            k
        })
        .collect();
    seen.sort_unstable();
    seen.dedup();
    assert_eq!(seen.len(), 3200, "no triangle is duplicated or lost");
    Ok(())
}

/// A deterministic unit vector stream.
struct Directions(u64);

impl Directions {
    fn next_f32(&mut self) -> f32 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        ((self.0 >> 40) as f32 / (1u64 << 24) as f32) * 2.0 - 1.0
    }

    fn next(&mut self) -> [f32; 3] {
        loop {
            let v = [self.next_f32(), self.next_f32(), self.next_f32()];
            let len = dot(v, v).sqrt();
            if len > 0.1 && len <= 1.0 {
                return v.map(|c| c / len);
            }
        }
    }
}

#[test]
fn meshlet_spheres_contain_their_vertices_and_cones_are_conservative() -> TestResult {
    let mut dirs = Directions(7);
    let mut culled = 0usize;
    let mut closed_cones = 0usize;
    for (name, obj) in [
        ("sphere", sphere_obj(24, 48)),
        ("grid", grid_obj(30)),
        ("cube", cube_obj(true)),
    ] {
        let mesh = cook_one(&format!("meshes/{name}.obj"), &obj)?;
        for m in &mesh.meshlets {
            let range = m.first_index as usize..(m.first_index + m.index_count) as usize;
            let idx = &mesh.indices[range];
            for i in idx {
                let p = mesh.vertices[*i as usize].position;
                let d = sub(p, m.center);
                assert!(dot(d, d).sqrt() <= m.radius, "{name}: vertex outside its sphere");
            }
            if m.cone_axis == [0.0; 3] {
                continue;
            }
            closed_cones += 1;
            let normals: Vec<[f32; 3]> = idx
                .as_chunks::<3>()
                .0
                .iter()
                .map(|t| {
                    let [a, b, c] = t.map(|i| mesh.vertices[i as usize].position);
                    cross(sub(b, a), sub(c, a))
                })
                .collect();
            for _ in 0..400 {
                let v = dirs.next();
                if dot(v, m.cone_axis) >= m.cone_cutoff {
                    culled += 1;
                    for n in &normals {
                        // Back-facing: the view direction (toward the meshlet) runs along
                        // the normal, so no triangle is visible.
                        assert!(dot(v, *n) >= 0.0, "{name}: a visible triangle was culled");
                    }
                }
            }
        }
    }
    assert!(closed_cones > 0 && culled > 0, "the test must exercise culling");
    Ok(())
}

#[test]
fn sidecar_scales_and_selects_lightmap_uvs() -> TestResult {
    let obj = "v 0 0 0\nv 1 0 0\nv 0 1 0\nvt 0 0\nvt 1 0\nvt 0 1\nvt2 0.5 0.5\nvt2 0.75 0.5\nvt2 0.5 0.75\nf 1/1 2/2 3/3\n";
    let mut t = ContentTree::new();
    t.insert("meshes/sign.obj", obj);
    t.insert("meshes/sign.obj.toml", "scale = 2.0\nlightmap_uv = true\n");
    t.insert("meshes/plain.obj", obj);
    let out = run(&t)?;
    let sign = MeshAsset::parse(&out.get("meshes/sign.mesh").ok_or("sign")?.bytes)?;
    assert_eq!(sign.bounds_max, [2.0, 2.0, 0.0]);
    assert!(
        sign.vertices
            .iter()
            .any(|v| v.position == [2.0, 0.0, 0.0] && v.uv1 == [0.75, 0.5])
    );
    let plain = MeshAsset::parse(&out.get("meshes/plain.mesh").ok_or("plain")?.bytes)?;
    assert!(plain.vertices.iter().all(|v| v.uv1 == [0.0, 0.0]));
    assert!(
        out.get("meshes/sign.obj.mesh").is_none(),
        "the sidecar is an input"
    );
    // Asked for lightmap UVs without a second set: none are generated.
    let mut t = ContentTree::new();
    t.insert("meshes/bare.obj", "v 0 0 0\nv 1 0 0\nv 0 1 0\nf 1 2 3\n");
    t.insert("meshes/bare.obj.toml", "lightmap_uv = true\n");
    let bare = MeshAsset::parse(&run(&t)?.get("meshes/bare.mesh").ok_or("bare")?.bytes)?;
    assert!(bare.vertices.iter().all(|v| v.uv1 == [0.0, 0.0]));
    Ok(())
}

const SKIN_OBJ: &str = "v 0 0 0\nv 1 0 0\nv 1 1 0\nv 0 1 0\nf 1 2 3 4\n";

#[test]
fn skinned_weights_quantize_to_exactly_255() -> TestResult {
    let mut t = ContentTree::new();
    t.insert("meshes/arm.skin.obj", SKIN_OBJ);
    t.insert(
        "meshes/arm.skinmesh.toml",
        "mesh = \"meshes/arm.skin.obj\"\n\
         [weights.1]\njoints = [0, 1, 2]\nweights = [0.333333, 0.333333, 0.333334]\n\
         [weights.2]\njoints = [0, 1]\nweights = [0.5, 0.5]\n\
         [weights.3]\njoints = [3, 2, 1, 0]\nweights = [0.1, 0.2, 0.3, 0.4]\n\
         [weights.4]\njoints = [1]\nweights = [1]\n",
    );
    let out = run(&t)?;
    assert!(
        out.get("meshes/arm.skin.mesh").is_none(),
        "skinned geometry is not cooked alone"
    );
    let asset = out.get("meshes/arm.skinmesh").ok_or("skinned output")?;
    assert_eq!(asset.kind, AssetKind::Mesh);
    let mesh = MeshAsset::parse(&asset.bytes)?;
    let skin = mesh.skin.as_ref().ok_or("skin stream")?;
    assert_eq!(skin.len(), mesh.vertices.len());
    for s in skin {
        assert_eq!(s.weights.iter().map(|w| u32::from(*w)).sum::<u32>(), 255, "{s:?}");
    }
    let at = |p: [f32; 3]| {
        mesh.vertices
            .iter()
            .position(|v| v.position == p)
            .map(|i| skin[i])
            .ok_or("vertex")
    };
    assert_eq!(at([0.0, 0.0, 0.0])?.weights, [85, 85, 85, 0]);
    assert_eq!(at([1.0, 0.0, 0.0])?.weights[..2], [128, 127]);
    let third = at([1.0, 1.0, 0.0])?;
    assert_eq!(third.joints, [3, 2, 1, 0]);
    for (q, w) in third.weights.iter().zip([0.1f32, 0.2, 0.3, 0.4]) {
        assert!((f32::from(*q) - w * 255.0).abs() <= 1.0, "{third:?}");
    }
    assert_eq!(at([0.0, 1.0, 0.0])?.weights, [255, 0, 0, 0]);
    Ok(())
}

#[test]
fn errors_name_the_file_and_line() -> TestResult {
    let cases: &[(&str, &str, &str)] = &[
        (
            "meshes/a.obj",
            "v 0 0 0\nv 1 0 0\nv 0 1 0\nf 1 2 4\n",
            "meshes/a.obj:4:",
        ),
        (
            "meshes/b.obj",
            "v 0 0 0\nv 1 0 0\nv 0 1 0\nf 0 1 2\n",
            "meshes/b.obj:4:",
        ),
        ("meshes/c.obj", "v 0 0 0\nv 1 0 0\nf -3 1 2\n", "meshes/c.obj:3:"),
        ("meshes/d.obj", "v 0 0 0\nl 1 2\n", "meshes/d.obj:2:"),
        ("meshes/e.obj", "v 0 0 0\nvn 0 0 0\n", "meshes/e.obj:2:"),
        ("meshes/f.obj", "v 0 0 x\n", "meshes/f.obj:1:"),
        ("meshes/g.obj", "v 0 0 0\nv 1 0 0\nf 1 2\n", "meshes/g.obj:3:"),
        (
            "meshes/h.obj",
            "v 0 0 0\nv 1 0 0\nv 0 1 0\nvt 0 0\nf 1/2 2/1 3/1\n",
            "meshes/h.obj:5:",
        ),
    ];
    for (path, text, expect) in cases {
        let mut t = ContentTree::new();
        t.insert(path, *text);
        let shown = errors(&t)?;
        assert!(shown.iter().any(|e| e.starts_with(expect)), "{path}: {shown:?}");
    }
    let mut t = ContentTree::new();
    t.insert("meshes/k.obj", "v 0 0 0\nv 1 0 0\nv 0 1 0\nf 1 2 3\n");
    t.insert("meshes/k.obj.toml", "scale = 1.0\ncolor = 2\n");
    let shown = errors(&t)?;
    assert!(
        shown.iter().any(|e| e.starts_with("meshes/k.obj.toml:2:")),
        "{shown:?}"
    );
    let mut t = ContentTree::new();
    t.insert("meshes/k.obj", "v 0 0 0\nv 1 0 0\nv 0 1 0\nf 1 2 3\n");
    t.insert("meshes/k.obj.toml", "scale = -1.0\n");
    let shown = errors(&t)?;
    assert!(
        shown.iter().any(|e| e.starts_with("meshes/k.obj.toml:1:")),
        "{shown:?}"
    );
    // Skinned meshes: a missing table, weights that do not sum to 1, a bad vertex number,
    // and mismatched counts.
    let skin_cases: &[(&str, &str)] = &[
        (
            "mesh = \"meshes/s.skin.obj\"\n[weights.1]\njoints = [0]\nweights = [1.0]\n",
            "meshes/s.skinmesh.toml:1:",
        ),
        (
            "mesh = \"meshes/s.skin.obj\"\n[weights.1]\njoints = [0, 1]\nweights = [0.5, 0.6]\n",
            "meshes/s.skinmesh.toml:4:",
        ),
        (
            "mesh = \"meshes/s.skin.obj\"\n[weights.9]\njoints = [0]\nweights = [1.0]\n",
            "meshes/s.skinmesh.toml:2:",
        ),
        (
            "mesh = \"meshes/s.skin.obj\"\n[weights.1]\njoints = [0, 1]\nweights = [1.0]\n",
            "meshes/s.skinmesh.toml:4:",
        ),
        (
            "mesh = \"meshes/s.skin.obj\"\n[weights.1]\njoints = [0, 0]\nweights = [0.5, 0.5]\n",
            "meshes/s.skinmesh.toml:3:",
        ),
        ("mesh = \"meshes/missing.obj\"\n", "meshes/s.skinmesh.toml:1:"),
    ];
    for (text, expect) in skin_cases {
        let mut t = ContentTree::new();
        t.insert("meshes/s.skin.obj", SKIN_OBJ);
        t.insert("meshes/s.skinmesh.toml", *text);
        let shown = errors(&t)?;
        assert!(shown.iter().any(|e| e.starts_with(expect)), "{text}: {shown:?}");
    }
    Ok(())
}

#[test]
fn cooking_is_deterministic() -> TestResult {
    let tree = || {
        let mut t = ContentTree::new();
        t.insert("meshes/sphere.obj", sphere_obj(16, 32));
        t.insert("meshes/cube.obj", cube_obj(true));
        t.insert("meshes/grid.obj", grid_obj(20));
        t.insert("meshes/arm.skin.obj", SKIN_OBJ);
        t.insert(
            "meshes/arm.skinmesh.toml",
            "mesh = \"meshes/arm.skin.obj\"\n[weights.1]\njoints = [0]\nweights = [1]\n\
             [weights.2]\njoints = [0]\nweights = [1]\n[weights.3]\njoints = [1]\nweights = [1]\n\
             [weights.4]\njoints = [1]\nweights = [1]\n",
        );
        t
    };
    let a = run(&tree())?;
    let b = run(&tree())?;
    assert_eq!(a.assets.len(), 4);
    for (name, asset) in &a.assets {
        assert_eq!(Some(&asset.bytes), b.get(name).map(|x| &x.bytes), "{name}");
    }
    assert_eq!(
        a.bundle(Domain::Presentation, 1).hash(),
        b.bundle(Domain::Presentation, 1).hash()
    );
    Ok(())
}
