//! The sector bake end to end: probe volumes against analytic sky irradiance, occlusion,
//! invalid probes, lightmap shadows, atlas layout, located errors, the sector importer
//! picking the bakes up, and determinism. Meshes come from a test importer of synthetic
//! `*.test_mesh` sources.

#![allow(
    clippy::indexing_slicing,
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss
)]

use std::fmt::Write as _;
use std::sync::Arc;

use mantis_cook::importer::{CookError, Cooked, ImportContext, Importer, Source};
use mantis_cook::importers::world::LIGHTMAP_LAYOUT_KIND;
use mantis_cook::tree::ContentTree;
use mantis_cook::{Cook, CookOutput, importers};
use mantis_formats::bundle::{AssetKind, Domain};
use mantis_formats::lightmap::{Lightmap, rgb9e5_to_rgb};
use mantis_formats::mesh::{MeshAsset, MeshVertex, Meshlet};
use mantis_formats::probe_volume::ProbeVolume;
use mantis_formats::sector::{PLACEMENT_LIGHTMAPPED, Sector};
use mantis_formats::sh::{ShL1, Y0, Y1};

type TestResult = Result<(), Box<dyn std::error::Error>>;

/// Phase 0: `*.test_mesh` sources (one line, see [`mesh_from`]) cook to MMSH, and
/// `*.test_material` sources to opaque material payloads.
struct Synthetic;

/// Four corners of a quad wound so `(b - a) x (c - a)` points along `outward`.
fn quad(corners: [[f32; 3]; 4], outward: [f32; 3], uv: [[f32; 2]; 4]) -> ([MeshVertex; 4], [u32; 6]) {
    let sub = |p: [f32; 3], q: [f32; 3]| [p[0] - q[0], p[1] - q[1], p[2] - q[2]];
    let (e1, e2) = (sub(corners[1], corners[0]), sub(corners[2], corners[0]));
    let n = [
        e1[1] * e2[2] - e1[2] * e2[1],
        e1[2] * e2[0] - e1[0] * e2[2],
        e1[0] * e2[1] - e1[1] * e2[0],
    ];
    let flip = n[0] * outward[0] + n[1] * outward[1] + n[2] * outward[2] < 0.0;
    let vertices = [0, 1, 2, 3].map(|i| MeshVertex {
        position: corners[i],
        normal: outward,
        uv0: uv[i],
        uv1: uv[i],
    });
    let indices = if flip {
        [0, 2, 1, 0, 3, 2]
    } else {
        [0, 1, 2, 0, 2, 3]
    };
    (vertices, indices)
}

fn finish(vertices: Vec<MeshVertex>, indices: Vec<u32>) -> MeshAsset {
    let mut lo = [f32::MAX; 3];
    let mut hi = [f32::MIN; 3];
    for v in &vertices {
        for k in 0..3 {
            lo[k] = lo[k].min(v.position[k]);
            hi[k] = hi[k].max(v.position[k]);
        }
    }
    let center = [0, 1, 2].map(|k| lo[k].midpoint(hi[k]));
    let radius = vertices
        .iter()
        .map(|v| {
            let d = [0, 1, 2].map(|k| v.position[k] - center[k]);
            (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt()
        })
        .fold(0.0f32, f32::max);
    let meshlets = indices
        .chunks(124 * 3)
        .scan(0u32, |first, chunk| {
            let m = Meshlet {
                first_index: *first,
                index_count: chunk.len() as u32,
                center,
                radius,
                cone_axis: [0.0; 3],
                cone_cutoff: 1.0,
            };
            *first += chunk.len() as u32;
            Some(m)
        })
        .collect();
    MeshAsset {
        vertices,
        skin: None,
        indices,
        meshlets,
        bounds_min: lo,
        bounds_max: hi,
    }
}

/// A mesh from one line of text:
///
/// - `plane <sx> <sz> [uv1 scale]`: a quad on y = 0 centered on the origin, facing +Y,
///   `uv1 = ((x + sx / 2) / sx, (z + sz / 2) / sz) * scale` (scale 1 by default).
/// - `box <sx> <sy> <sz>`: a closed box, x and z centered, y from 0 to `sy`, faces
///   outward, each face in its own cell of a 3 x 2 `uv1` grid.
fn mesh_from(text: &str) -> Option<MeshAsset> {
    let words: Vec<&str> = text.split_whitespace().collect();
    let nums: Vec<f32> = words.iter().skip(1).filter_map(|w| w.parse().ok()).collect();
    let mut vertices = Vec::new();
    let mut indices = Vec::new();
    let mut push = |(vs, is): ([MeshVertex; 4], [u32; 6])| {
        let base = vertices.len() as u32;
        vertices.extend(vs);
        indices.extend(is.map(|i| i + base));
    };
    match (words.first().copied()?, nums.as_slice()) {
        ("plane", [sx, sz, rest @ ..]) => {
            let k = rest.first().copied().unwrap_or(1.0);
            let (hx, hz) = (sx / 2.0, sz / 2.0);
            push(quad(
                [[-hx, 0.0, -hz], [-hx, 0.0, hz], [hx, 0.0, hz], [hx, 0.0, -hz]],
                [0.0, 1.0, 0.0],
                [[0.0, 0.0], [0.0, k], [k, k], [k, 0.0]],
            ));
        }
        ("box", [sx, sy, sz]) => {
            let (x0, x1, y0, y1, z0, z1) = (-sx / 2.0, sx / 2.0, 0.0, *sy, -sz / 2.0, sz / 2.0);
            let faces: [([[f32; 3]; 4], [f32; 3]); 6] = [
                (
                    [[x1, y0, z0], [x1, y1, z0], [x1, y1, z1], [x1, y0, z1]],
                    [1.0, 0.0, 0.0],
                ),
                (
                    [[x0, y0, z0], [x0, y1, z0], [x0, y1, z1], [x0, y0, z1]],
                    [-1.0, 0.0, 0.0],
                ),
                (
                    [[x0, y1, z0], [x1, y1, z0], [x1, y1, z1], [x0, y1, z1]],
                    [0.0, 1.0, 0.0],
                ),
                (
                    [[x0, y0, z0], [x1, y0, z0], [x1, y0, z1], [x0, y0, z1]],
                    [0.0, -1.0, 0.0],
                ),
                (
                    [[x0, y0, z1], [x1, y0, z1], [x1, y1, z1], [x0, y1, z1]],
                    [0.0, 0.0, 1.0],
                ),
                (
                    [[x0, y0, z0], [x1, y0, z0], [x1, y1, z0], [x0, y1, z0]],
                    [0.0, 0.0, -1.0],
                ),
            ];
            for (i, (corners, outward)) in faces.into_iter().enumerate() {
                let (cu, cv) = ((i % 3) as f32 / 3.0, (i / 3) as f32 / 2.0);
                let (du, dv) = (1.0 / 3.0 - 0.02, 0.5 - 0.02);
                let (u0, v0) = (cu + 0.01, cv + 0.01);
                push(quad(
                    corners,
                    outward,
                    [[u0, v0], [u0 + du, v0], [u0 + du, v0 + dv], [u0, v0 + dv]],
                ));
            }
        }
        _ => return None,
    }
    Some(finish(vertices, indices))
}

impl Importer for Synthetic {
    fn name(&self) -> &'static str {
        "test.synthetic"
    }
    fn version(&self) -> u32 {
        1
    }
    fn phase(&self) -> u32 {
        0
    }
    fn accepts(&self, path: &str) -> bool {
        path.ends_with(".test_mesh") || path.ends_with(".test_material")
    }
    fn import(&self, s: &Source<'_>, _ctx: &ImportContext<'_>) -> Result<Vec<Cooked>, CookError> {
        if s.path.ends_with(".test_material") {
            return Ok(vec![Cooked {
                name: format!("{}.material", s.stem_path()),
                kind: AssetKind::Material,
                domain: Domain::Presentation,
                bytes: s.bytes.to_vec(),
            }]);
        }
        let mesh = mesh_from(s.text()?).ok_or_else(|| CookError::at(s.path, 1, "bad test mesh"))?;
        let bytes = mesh.encode();
        MeshAsset::parse(&bytes).map_err(|e| CookError::at(s.path, 1, &format!("{e}")))?;
        Ok(vec![Cooked {
            name: format!("{}.mesh", s.stem_path()),
            kind: AssetKind::Mesh,
            domain: Domain::Presentation,
            bytes,
        }])
    }
}

/// Cooks `tree` with the built-in importers and [`Synthetic`].
fn cook(tree: &ContentTree) -> Result<CookOutput, Vec<CookError>> {
    let mut all = importers::builtin();
    all.push(Arc::new(Synthetic));
    Cook::new(all).map_err(|e| vec![e])?.run(tree)
}

/// A tree with the synthetic meshes and material every bake test uses.
fn base_tree() -> ContentTree {
    let mut t = ContentTree::new();
    t.insert("meshes/floor.test_mesh", "plane 8 8");
    t.insert("meshes/crate.test_mesh", "box 2 2 2");
    t.insert("meshes/bad_uv.test_mesh", "plane 4 4 2");
    t.insert("materials/stone.test_material", "material");
    t
}

/// The lightmap layout payload as `(name, [scale u, scale v, offset u, offset v])`.
fn layout(text: &str) -> Vec<(String, [f32; 4])> {
    text.lines()
        .filter_map(|l| {
            let mut it = l.split_whitespace();
            let name = it.next()?.to_owned();
            let v: Vec<f32> = it.filter_map(|t| t.parse().ok()).collect();
            Some((name, <[f32; 4]>::try_from(v.as_slice()).ok()?))
        })
        .collect()
}

/// The texel of `lightmap` layer `layer` at a placement's `uv1`, via its layout rect.
fn texel_at(lightmap: &Lightmap, rect: [f32; 4], uv1: [f32; 2], layer: usize) -> Option<[f32; 3]> {
    let [scale_u, scale_v, offset_u, offset_v] = rect;
    let atlas = [uv1[0] * scale_u + offset_u, uv1[1] * scale_v + offset_v];
    let column = (atlas[0] * lightmap.width as f32).floor() as usize;
    let row = (atlas[1] * lightmap.height as f32).floor() as usize;
    let texel = lightmap
        .layers
        .get(layer)?
        .get(row * lightmap.width as usize + column)?;
    Some(rgb9e5_to_rgb(*texel))
}

const HEADER: &str = "[sector]\nx = 0\nz = 0\nsize = 16.0\n";
const GROUND: &str = "\n[ground]\ncell = 4.0\nflat = 0.0\n";
const NOON: &str =
    "\n[bake.noon]\ntime = 0.5\nsun_direction = [0.0, -1.0, 0.0]\nsun_color = [2.0, 2.0, 2.0]\n";

/// A sector source: header, optional ground, `body` (placements, hulls), `[bake]` keys,
/// and the noon keyframe plus `keyframes`.
fn source(ground: bool, body: &str, bake: &str, keyframes: &str) -> String {
    format!(
        "{HEADER}{}{body}\n[bake]\n{bake}\n{NOON}{keyframes}",
        if ground { GROUND } else { "" }
    )
}

fn tree_with(sector: &str) -> ContentTree {
    let mut t = base_tree();
    t.insert("sectors/0_0.sector.toml", sector);
    t
}

fn probes_of(out: &CookOutput) -> Result<ProbeVolume, Box<dyn std::error::Error>> {
    let asset = out.get("sectors/0_0.probes").ok_or("no probe volume")?;
    assert_eq!(asset.kind, AssetKind::ProbeVolume);
    assert_eq!(asset.domain, Domain::Presentation);
    Ok(ProbeVolume::parse(&asset.bytes)?)
}

fn line_of(text: &str, needle: &str) -> usize {
    text.lines().position(|l| l.contains(needle)).map_or(0, |i| i + 1)
}

fn expect_error(sector: &str, needle: &str, what: &str) -> TestResult {
    let errors = cook(&tree_with(sector))
        .err()
        .ok_or(format!("{what} must fail"))?;
    let line = line_of(sector, needle);
    assert!(
        errors
            .iter()
            .any(|e| e.file == "sectors/0_0.sector.toml" && e.line == line),
        "{what}: expected line {line}, got {errors:?}"
    );
    Ok(())
}

const SIX: [[f32; 3]; 6] = [
    [1.0, 0.0, 0.0],
    [-1.0, 0.0, 0.0],
    [0.0, 1.0, 0.0],
    [0.0, -1.0, 0.0],
    [0.0, 0.0, 1.0],
    [0.0, 0.0, -1.0],
];

fn max_error(probes: &[ShL1], expected: [f32; 4]) -> f32 {
    probes
        .iter()
        .flat_map(|p| p.rgb.iter())
        .flat_map(|c| c.iter().zip(expected).map(|(a, b)| (a - b).abs()))
        .fold(0.0, f32::max)
}

#[test]
fn open_air_probes_match_the_analytic_sphere() -> TestResult {
    let sector = source(
        false,
        "",
        "samples = 1024\nbounces = 0\nprobe_height = 4.0\nsky_color = [1.0, 1.0, 1.0]\nground_color = [1.0, 1.0, 1.0]",
        "",
    );
    let out = cook(&tree_with(&sector)).map_err(|e| format!("{e:?}"))?;
    let v = probes_of(&out)?;
    assert_eq!(v.dims, [5, 2, 5]);
    assert_eq!(v.origin, [0.0, 0.5, 0.0]);
    assert_eq!(v.valid, None, "nothing is inside geometry");
    // Uniform unit radiance over the whole sphere: L00 = 4 pi Y00, L1 = 0, E / pi = 1.
    let analytic = [4.0 * core::f32::consts::PI * Y0, 0.0, 0.0, 0.0];
    let frame = v.probes.first().ok_or("keyframe")?;
    let err = max_error(frame, analytic);
    let probe = frame.first().ok_or("probe")?;
    eprintln!(
        "open air: analytic {analytic:?}, baked {:?}, max abs error {err}",
        probe.rgb[0]
    );
    assert!(err < 0.01, "{err}");
    for n in SIX {
        let e = probe.irradiance(n);
        assert!(e.iter().all(|c| (c - 1.0).abs() < 0.01), "{n:?}: {e:?}");
    }
    Ok(())
}

#[test]
fn probes_over_ground_see_the_analytic_hemisphere() -> TestResult {
    let sector = source(
        true,
        "",
        "samples = 1024\nbounces = 0\nprobe_height = 4.0\nground_color = [0.0, 0.0, 0.0]",
        "",
    );
    let out = cook(&tree_with(&sector)).map_err(|e| format!("{e:?}"))?;
    let v = probes_of(&out)?;
    let pi = core::f32::consts::PI;
    // Unit radiance over the upper hemisphere: L00 = 2 pi Y00, L1-1 = pi Y1 (y up).
    let analytic = [2.0 * pi * Y0, pi * Y1, 0.0, 0.0];
    let frame = v.probes.first().ok_or("keyframe")?;
    let err = max_error(frame, analytic);
    let probe = frame.get(v.index(2, 0, 2)).ok_or("probe")?;
    let [up, side, down] =
        [[0.0, 1.0, 0.0], [1.0, 0.0, 0.0], [0.0, -1.0, 0.0]].map(|n| probe.irradiance(n)[0]);
    eprintln!(
        "hemisphere: analytic {analytic:?}, baked {:?}, max abs error {err}; E/pi up {up} (1), side {side} (0.5), down {down} (0)",
        probe.rgb[0]
    );
    assert!(err < 0.01, "{err}");
    assert!((up - 1.0).abs() < 0.01 && (side - 0.5).abs() < 0.01 && down.abs() < 0.01);
    Ok(())
}

#[test]
fn a_roof_hides_the_sky_and_a_solid_box_invalidates_probes() -> TestResult {
    let body = "\n[hull.roof]\nmin = [1.0, 3.0, 1.0]\nmax = [7.0, 3.5, 7.0]\n\n[hull.block]\nmin = [10.0, -1.0, 10.0]\nmax = [14.0, 6.0, 14.0]\n";
    let sector = source(true, body, "samples = 256\nbounces = 0\nprobe_height = 4.0", "");
    let out = cook(&tree_with(&sector)).map_err(|e| format!("{e:?}"))?;
    let v = probes_of(&out)?;
    let frame = v.probes.first().ok_or("keyframe")?;
    let up = |x, y, z| {
        frame
            .get(v.index(x, y, z))
            .map_or(f32::NAN, |p| p.irradiance([0.0, 1.0, 0.0])[0])
    };
    // (4, 0.5, 4) is under the roof; (0, 0.5, 16) is in the open corner.
    let (covered, open) = (up(1, 0, 1), up(0, 0, 4));
    eprintln!("sky irradiance up: under the roof {covered}, open {open}");
    assert!(open > 0.95, "{open}");
    assert!(covered < 0.6 * open, "{covered} vs {open}");
    let valid = v.valid.as_ref().ok_or("a validity mask")?;
    for z in 0..5 {
        for y in 0..2 {
            for x in 0..5 {
                let inside = x == 3 && z == 3; // (12, *, 12) is inside the block
                assert_eq!(valid[v.index(x, y, z)], !inside, "probe {x} {y} {z}");
            }
        }
    }
    assert_eq!(frame[v.index(3, 0, 3)], ShL1::ZERO, "invalid probes are zero");
    Ok(())
}

#[test]
fn the_bounce_adds_light_from_sunlit_ground() -> TestResult {
    let bake = "samples = 256\nprobe_height = 4.0\nground_color = [0.0, 0.0, 0.0]\nalbedo = [0.5, 0.5, 0.5]";
    let dark = cook(&tree_with(&source(true, "", &format!("{bake}\nbounces = 0"), "")))
        .map_err(|e| format!("{e:?}"))?;
    let lit = cook(&tree_with(&source(true, "", &format!("{bake}\nbounces = 1"), "")))
        .map_err(|e| format!("{e:?}"))?;
    let down = |out: &CookOutput| -> Result<f32, Box<dyn std::error::Error>> {
        let v = probes_of(out)?;
        let p = v.probes[0].get(v.index(2, 0, 2)).ok_or("probe")?;
        Ok(p.irradiance([0.0, -1.0, 0.0])[0])
    };
    let (without, with) = (down(&dark)?, down(&lit)?);
    // The ground below is sunlit at E/pi = 2 with albedo 0.5: radiance 1 from below.
    eprintln!("downward E/pi: no bounce {without}, one bounce {with} (analytic 1)");
    assert!(
        without.abs() < 0.01 && (with - 1.0).abs() < 0.05,
        "{without} {with}"
    );
    Ok(())
}

const SHADE: &str = "\n[placement.floor]\nmesh = \"meshes/floor.test_mesh\"\nmaterial = \"materials/stone.test_material\"\nposition = [8.0, 0.01, 8.0]\nlightmapped = true\n\n[hull.awning]\nmin = [4.0, 2.0, 4.0]\nmax = [8.0, 2.5, 12.0]\n";

fn shade_sector() -> String {
    source(
        true,
        SHADE,
        "samples = 64\nbounces = 0\nlightmap_texels_per_meter = 2.0",
        "\n[bake.dusk]\ntime = 0.75\nsun_direction = [1.0, -1.0, 0.0]\nsun_color = [1.0, 0.5, 0.25]\n",
    )
}

#[test]
fn lightmap_texels_in_shadow_are_darker() -> TestResult {
    let sector = shade_sector();
    let out = cook(&tree_with(&sector)).map_err(|e| format!("{e:?}"))?;
    let asset = out.get("sectors/0_0.lightmap").ok_or("no lightmap")?;
    assert_eq!(
        (asset.kind, asset.domain),
        (AssetKind::Lightmap, Domain::Presentation)
    );
    let lm = Lightmap::parse(&asset.bytes)?;
    assert_eq!(lm.keyframes, vec![0.5, 0.75]);
    let layout_asset = out.get("sectors/0_0.lightmap_layout").ok_or("no layout")?;
    assert_eq!(
        (layout_asset.kind, layout_asset.domain),
        (LIGHTMAP_LAYOUT_KIND, Domain::Presentation)
    );
    let rects = layout(core::str::from_utf8(&layout_asset.bytes)?);
    let [(name, rect)] = rects.as_slice() else {
        return Err(format!("{rects:?}").into());
    };
    assert_eq!(name, "floor");
    // Floor uv1 u = (x - 4) / 8: x = 6 lies under the awning (x 4 to 8), x = 10 in the sun.
    let shaded = texel_at(&lm, *rect, [0.25, 0.5], 0).ok_or("texel")?;
    let lit = texel_at(&lm, *rect, [0.75, 0.5], 0).ok_or("texel")?;
    eprintln!("noon lightmap E/pi: shadowed {shaded:?}, lit {lit:?}");
    assert!(lit[0] > shaded[0] + 1.5, "{lit:?} vs {shaded:?}");
    // Lit: sun 2 plus most of the sky (at most 1); shadowed: sky only.
    assert!(
        lit[0] > 2.0 && lit[0] < 3.0 && shaded[0] < 1.0,
        "{lit:?} {shaded:?}"
    );
    // The dusk sun travels toward +X at 45 degrees: the awning's shadow moves toward +X.
    let dusk_lit = texel_at(&lm, *rect, [0.05, 0.5], 1).ok_or("texel")?;
    let dusk_shaded = texel_at(&lm, *rect, [0.6, 0.5], 1).ok_or("texel")?;
    eprintln!("dusk lightmap E/pi: shadowed {dusk_shaded:?}, lit {dusk_lit:?}");
    assert!(
        dusk_lit[0] > dusk_shaded[0] + 0.5,
        "{dusk_lit:?} vs {dusk_shaded:?}"
    );
    Ok(())
}

#[test]
fn atlas_rectangles_never_overlap_and_stay_inside() -> TestResult {
    let mut body = String::new();
    for i in 0..12 {
        let (x, z) = (1.0 + (i % 4) as f32 * 4.0, 2.0 + (i / 4) as f32 * 5.0);
        let scale = 0.25 + (i % 5) as f32 * 0.3;
        write!(
            body,
            "\n[placement.crate_{i:02}]\nmesh = \"meshes/crate.test_mesh\"\nmaterial = \"materials/stone.test_material\"\nposition = [{x:.1}, 0.0, {z:.1}]\nscale = {scale:.2}\nyaw = {}.0\nlightmapped = true\n",
            i * 17
        )?;
    }
    let sector = source(
        true,
        &body,
        "samples = 16\nbounces = 0\nlightmap_texels_per_meter = 6.0\nlightmap_max_size = 64\nlightmap_padding = 2",
        "",
    );
    let out = cook(&tree_with(&sector)).map_err(|e| format!("{e:?}"))?;
    let lm = Lightmap::parse(&out.get("sectors/0_0.lightmap").ok_or("lightmap")?.bytes)?;
    assert!(lm.width <= 64 && lm.height <= 64, "{}x{}", lm.width, lm.height);
    let rects = layout(core::str::from_utf8(
        &out.get("sectors/0_0.lightmap_layout").ok_or("layout")?.bytes,
    )?);
    assert_eq!(rects.len(), 12);
    let (w, h) = (lm.width as f32, lm.height as f32);
    // Padded texel rectangles: [x0, y0, x1, y1).
    let padded: Vec<[f32; 4]> = rects
        .iter()
        .map(|(_, [su, sv, ou, ov])| {
            let (x0, y0) = (ou * w, ov * h);
            [x0 - 2.0, y0 - 2.0, x0 + su * w + 2.0, y0 + sv * h + 2.0]
        })
        .collect();
    for (i, a) in padded.iter().enumerate() {
        assert!(
            a[0] >= -1e-3 && a[1] >= -1e-3 && a[2] <= w + 1e-3 && a[3] <= h + 1e-3,
            "{a:?} outside {w}x{h}"
        );
        assert!(
            a.iter().all(|c| (c - c.round()).abs() < 1e-3),
            "{a:?} is not on texel edges"
        );
        for b in padded.iter().skip(i + 1) {
            let apart =
                a[2] <= b[0] + 1e-3 || b[2] <= a[0] + 1e-3 || a[3] <= b[1] + 1e-3 || b[3] <= a[1] + 1e-3;
            assert!(apart, "{a:?} overlaps {b:?}");
        }
    }
    Ok(())
}

#[test]
fn lightmap_coordinates_outside_the_unit_square_fail_at_the_placement() -> TestResult {
    let sector = shade_sector().replace("meshes/floor.test_mesh", "meshes/bad_uv.test_mesh");
    expect_error(&sector, "meshes/bad_uv.test_mesh", "uv1 up to 2")
}

#[test]
fn bake_settings_errors_name_the_line() -> TestResult {
    let base = shade_sector();
    let cases = [
        ("bounces = 0", "bounces = 2"),
        ("samples = 64", "samples = 64\nprobe_spcing = 1.0"),
        ("time = 0.75", "time = 1.5"),
        (
            "sun_direction = [1.0, -1.0, 0.0]",
            "sun_direction = [0.0, 0.0, 0.0]",
        ),
        ("samples = 64", "samples = 64\nprobe_spacing = 0.01"),
        ("samples = 64", "samples = 64\nalbedo = [2.0, 0.5, 0.5]"),
    ];
    for (from, to) in cases {
        let sector = base.replacen(from, to, 1);
        let needle = to.lines().last().unwrap_or(to);
        expect_error(&sector, needle, to)?;
    }
    // Two keyframes at one time: the error points at the second.
    let sector = base.replacen("time = 0.75", "time = 0.5", 1);
    let errors = cook(&tree_with(&sector))
        .err()
        .ok_or("duplicate times must fail")?;
    let line = sector
        .lines()
        .enumerate()
        .filter(|(_, l)| l.contains("time = 0.5"))
        .nth(1)
        .map_or(0, |(i, _)| i + 1);
    assert!(
        errors
            .iter()
            .any(|e| e.line == line && e.message.contains("share")),
        "{errors:?}"
    );
    Ok(())
}

#[test]
fn the_sector_picks_up_the_bakes() -> TestResult {
    let sector = format!(
        "{}\n[placement.crate]\nmesh = \"meshes/crate.test_mesh\"\nmaterial = \"materials/stone.test_material\"\nposition = [12.0, 0.0, 12.0]\n",
        shade_sector()
    );
    let out = cook(&tree_with(&sector)).map_err(|e| format!("{e:?}"))?;
    let probes = out.get("sectors/0_0.probes").ok_or("probes")?;
    let atlas = out.get("sectors/0_0.lightmap").ok_or("lightmap")?;
    let rects = layout(core::str::from_utf8(
        &out.get("sectors/0_0.lightmap_layout").ok_or("layout")?.bytes,
    )?);
    ProbeVolume::parse(&probes.bytes)?;
    Lightmap::parse(&atlas.bytes)?;
    for name in ["sectors/0_0.sector", "sectors/0_0.server.sector"] {
        let s = Sector::parse(&out.get(name).ok_or("sector")?.bytes)?;
        assert_eq!(
            (s.probe_volume, &s.lightmaps, &s.placements),
            (None, &None, &None),
            "{name}: lighting and placements are visual (decision 0020)"
        );
    }
    {
        let s = Sector::parse(&out.get("sectors/0_0.visual").ok_or("visual")?.bytes)?;
        assert_eq!(s.probe_volume, Some(probes.hash), "PRBV names the probe volume");
        assert_eq!(s.lightmaps, Some(vec![atlas.hash]), "LMAP names the atlas");
        let placements = s.placements.ok_or("placements")?;
        let [floor, plain] = placements.as_slice() else {
            return Err("two placements".into());
        };
        let [su, sv, ou, ov] = rects.iter().find(|(n, _)| n == "floor").ok_or("floor rect")?.1;
        assert!(floor.flags & PLACEMENT_LIGHTMAPPED != 0);
        assert_eq!(floor.lightmap, atlas.hash);
        assert_eq!((floor.uv_scale, floor.uv_offset), ([su, sv], [ou, ov]));
        assert_eq!(plain.flags & PLACEMENT_LIGHTMAPPED, 0);
        assert_eq!((plain.uv_scale, plain.uv_offset), ([0.0; 2], [0.0; 2]));
    }
    Ok(())
}

#[test]
fn sectors_without_a_bake_table_produce_no_bakes() -> TestResult {
    let sector = format!("{HEADER}{GROUND}");
    let out = cook(&tree_with(&sector)).map_err(|e| format!("{e:?}"))?;
    assert!(
        out.assets
            .keys()
            .all(|k| !k.ends_with(".probes") && !k.contains(".lightmap"))
    );
    let s = Sector::parse(&out.get("sectors/0_0.visual").ok_or("visual")?.bytes)?;
    assert_eq!((s.probe_volume, s.lightmaps), (None, None));
    Ok(())
}

#[test]
fn baking_is_deterministic() -> TestResult {
    let sector = shade_sector().replace("bounces = 0", "bounces = 1");
    let a = cook(&tree_with(&sector)).map_err(|e| format!("{e:?}"))?;
    let b = cook(&tree_with(&sector)).map_err(|e| format!("{e:?}"))?;
    for name in [
        "sectors/0_0.probes",
        "sectors/0_0.lightmap",
        "sectors/0_0.lightmap_layout",
    ] {
        let (x, y) = (a.get(name).ok_or(name)?, b.get(name).ok_or(name)?);
        assert_eq!(x.bytes, y.bytes, "{name}");
    }
    assert_eq!(
        a.bundle(Domain::Presentation, 1).hash(),
        b.bundle(Domain::Presentation, 1).hash()
    );
    Ok(())
}
