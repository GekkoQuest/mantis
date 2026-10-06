//! Material importer: `*.material.toml` cooks to MMAT version 2 (graph plus texture and
//! parameter tables), every canonical permutation is valid WGSL under naga, every rule
//! fails at its file and line, texture references resolve by source path to the cooked
//! texture's hash and flags, and cooking is deterministic.

#![allow(clippy::too_many_lines)]

use std::sync::Arc;

use mantis_cook::importer::{CookError, Cooked, ImportContext, Importer, Source};
use mantis_cook::importers::content::material::check_wgsl;
use mantis_cook::tree::ContentTree;
use mantis_cook::{Cook, CookOutput, importers};
use mantis_formats::bundle::{AssetKind, Domain};
use mantis_formats::material::{
    ColorDefault, Deform, LightingModel, MaterialAsset, Node, NodeId, Pass, ScalarDefault, TEXTURE_SRGB,
    TextureRef,
};
use mantis_formats::texture::{Encoding, FLAG_SRGB, TextureAsset};

type TestResult = Result<(), Box<dyn std::error::Error>>;

/// Phase 0 stand-in for the texture importer: `*.testtex` cooks to a 1 x 1 RGBA8
/// texture of the source's first byte, sRGB unless the source starts with `linear`.
struct FakeTextures;

impl Importer for FakeTextures {
    fn name(&self) -> &'static str {
        "test.texture"
    }
    fn version(&self) -> u32 {
        1
    }
    fn phase(&self) -> u32 {
        0
    }
    fn accepts(&self, path: &str) -> bool {
        path.ends_with(".testtex")
    }
    fn import(&self, s: &Source<'_>, _ctx: &ImportContext<'_>) -> Result<Vec<Cooked>, CookError> {
        let value = s.bytes.first().copied().unwrap_or(0);
        let texture = TextureAsset {
            encoding: Encoding::Rgba8,
            flags: if s.bytes.starts_with(b"linear") {
                0
            } else {
                FLAG_SRGB
            },
            encoder_version: 0,
            width: 1,
            height: 1,
            mips: vec![vec![value; 4]],
        };
        Ok(vec![Cooked {
            name: format!("{}.tex", s.stem_path()),
            kind: AssetKind::Texture,
            domain: Domain::Presentation,
            bytes: texture.encode(),
        }])
    }
}

fn cook(tree: &ContentTree) -> Result<CookOutput, Vec<CookError>> {
    let mut all = importers::builtin();
    all.push(Arc::new(FakeTextures));
    Cook::new(all).map_err(|e| vec![e])?.run(tree)
}

const PATH: &str = "materials/crate.material.toml";

/// A toon character material: textured, tinted, rim, outline, shadows, skinned and
/// vertex-animated. Nodes are deliberately out of dependency order.
const TOON: &str = r#"# a toon material
casts_shadows = true
deformations = ["skinned", "vat"]
textures = ["textures/albedo.testtex"]

[lighting]
model = "toon"
bands = 3
softness = 0.2
shadow_tint = [0.55, 0.5, 0.7]

[rim]
color = [1.0, 0.9, 0.8]
power = 3
intensity = 0.6

[outline]
width_px = 2.0
color = [0.05, 0.03, 0.04]

[output]
base_color = "rgb"

[node.rgb]
op = "swizzle"
components = "xyz"
inputs = ["tinted"]

[node.tinted]
op = "multiply"
inputs = ["albedo", "tint"]

[node.uv]
op = "uv0"

[node.albedo]
op = "texture"
slot = 0
inputs = ["uv"]

[node.tint]
op = "color_param"
name = "tint"

[param.tint]
kind = "color"
default = [1.0, 0.9, 0.8]
"#;

fn tree_with(material: &str) -> ContentTree {
    let mut t = ContentTree::new();
    t.insert("textures/albedo.testtex", "albedo pixels");
    t.insert("textures/ramp.testtex", "linear ramp pixels");
    t.insert(PATH, material);
    t
}

fn line_of(text: &str, needle: &str) -> usize {
    text.lines().position(|l| l.contains(needle)).map_or(0, |i| i + 1)
}

/// Cooks `material` (edited from `TOON` by the caller) and returns the single error.
fn error_of(material: &str) -> Result<CookError, Box<dyn std::error::Error>> {
    let errors = cook(&tree_with(material))
        .err()
        .ok_or("expected the cook to fail")?;
    let [e] = <[CookError; 1]>::try_from(errors).map_err(|e| format!("expected one error: {e:?}"))?;
    Ok(e)
}

fn assert_at(material: &str, needle: &str, contains: &str) -> TestResult {
    let e = error_of(material)?;
    assert_eq!(e.file, PATH, "{e}");
    assert_eq!(e.line, line_of(material, needle), "{e}");
    assert!(e.message.contains(contains), "{e}");
    Ok(())
}

#[test]
fn a_toon_material_cooks_and_every_permutation_passes_naga() -> TestResult {
    let out = cook(&tree_with(TOON)).map_err(|e| format!("{e:?}"))?;
    let mat = out.get("materials/crate.mat").ok_or("material output")?;
    assert_eq!(mat.kind, AssetKind::Material);
    assert_eq!(mat.domain, Domain::Presentation);
    let asset = MaterialAsset::parse(&mat.bytes)?;
    assert!(asset.casts_shadows);
    assert!(asset.deformations.skinned && asset.deformations.vat);
    let g = &asset.graph.graph;
    // Inputs first, file order otherwise.
    let n = NodeId;
    assert_eq!(
        g.nodes,
        vec![
            Node::Uv0,
            Node::Texture { slot: 0, uv: n(0) },
            Node::ColorParam(0),
            Node::Multiply(n(1), n(2)),
            Node::Swizzle(n(3), [0, 1, 2, 0], 3),
        ]
    );
    assert_eq!(g.outputs.base_color, n(4));
    assert!(matches!(g.lighting, LightingModel::Toon { bands: 3, .. }));
    assert!(g.rim.is_some() && g.outline.is_some());
    let permutations = mantis_shadergen::compile_all(&asset)?;
    // 10 static + 8 skinned + 8 vertex animation.
    assert_eq!(permutations.len(), 26);
    assert!(permutations.iter().any(|p| p.key.deform() == Deform::Vat));
    assert!(permutations.iter().any(|p| p.key.pass() == Pass::Outline));
    for p in &permutations {
        check_wgsl(&p.source).map_err(|e| format!("{:?}: {e}", p.key))?;
    }
    // The binding tables: each slot's texture by content hash and flags, and the named
    // parameter with its default (alpha 1 for a three-number color).
    let albedo = out.get("textures/albedo.tex").ok_or("albedo")?;
    assert_eq!(
        asset.bindings.textures,
        vec![TextureRef {
            hash: albedo.hash,
            flags: TEXTURE_SRGB,
        }]
    );
    assert_eq!(
        asset.bindings.colors,
        vec![ColorDefault {
            name: "tint".to_owned(),
            value: [1.0, 0.9, 0.8, 1.0],
        }]
    );
    assert!(out.assets.keys().all(|k| !k.ends_with(".textures")), "no sidecar");
    Ok(())
}

/// Uses every op and the other lighting models.
#[test]
fn every_op_and_lighting_model_cooks_to_valid_wgsl() -> TestResult {
    let lambert = r#"
alpha_cutoff = 0.5
casts_shadows = false
textures = ["textures/albedo.testtex"]
[lighting]
model = "lambert"
[output]
base_color = "out"
alpha = "alpha"
emissive = "glow"
[node.n]
op = "world_normal"
[node.v]
op = "view_direction"
[node.ndv]
op = "dot"
inputs = ["n", "v"]
[node.sat]
op = "saturate"
inputs = ["ndv"]
[node.inv]
op = "one_minus"
inputs = ["sat"]
[param.p]
kind = "scalar"
default = [2.5]
[node.p]
op = "scalar_param"
name = "p"
[node.pow]
op = "power"
inputs = ["inv", "p"]
[node.c0]
op = "constant"
value = [0.2, 0.4, 0.8]
[node.c1]
op = "constant"
value = [1, 0.5, 0.25]
[node.mixed]
op = "lerp"
inputs = ["c0", "c1", "pow"]
[node.wp]
op = "world_position"
[node.wpn]
op = "normalize"
inputs = ["wp"]
[node.sum]
op = "add"
inputs = ["mixed", "wpn"]
[node.diff]
op = "subtract"
inputs = ["sum", "wpn"]
[node.out]
op = "divide"
inputs = ["diff", "p"]
[node.t]
op = "time"
[node.fr]
op = "fresnel"
inputs = ["p"]
[node.glow3]
op = "combine"
inputs = ["t", "fr", "sat"]
[node.glow]
op = "multiply"
inputs = ["glow3", "pow"]
[node.uv]
op = "uv1"
[node.tex]
op = "texture"
slot = 0
inputs = ["uv"]
[node.alpha]
op = "swizzle"
components = "a"
inputs = ["tex"]
"#;
    let ramp = r#"
textures = ["textures/albedo.testtex", "textures/ramp.testtex"]
[lighting]
model = "toon_ramp"
ramp_slot = 1
[output]
base_color = "c"
[node.c]
op = "constant"
value = [1, 1, 1, 1]
"#;
    let unlit = r#"
[lighting]
model = "unlit"
[output]
base_color = "rgb"
emissive = "rgb"
[param.col]
kind = "color"
default = [0.2, 0.4, 0.6, 1.0]
[node.col]
op = "color_param"
name = "col"
[node.rgb]
op = "swizzle"
components = "rgb"
inputs = ["col"]
"#;
    let mut t = tree_with(TOON);
    t.insert("materials/lambert.material.toml", lambert);
    t.insert("materials/ramp.material.toml", ramp);
    t.insert("materials/unlit.material.toml", unlit);
    let out = cook(&t).map_err(|e| format!("{e:?}"))?;
    let mut checked = 0;
    for name in [
        "materials/lambert.mat",
        "materials/ramp.mat",
        "materials/unlit.mat",
    ] {
        let asset = MaterialAsset::parse(&out.get(name).ok_or(name)?.bytes)?;
        for p in mantis_shadergen::compile_all(&asset)? {
            check_wgsl(&p.source).map_err(|e| format!("{name} {:?}: {e}", p.key))?;
            checked += 1;
        }
    }
    // Lambert with alpha test: forward 4 + depth 2 (no shadows); ramp: forward 4 +
    // depth 2 + shadow 2; unlit: forward 4 + depth 2 + shadow 2.
    assert_eq!(checked, 6 + 8 + 8);
    let lambert = MaterialAsset::parse(&out.get("materials/lambert.mat").ok_or("lambert")?.bytes)?;
    assert_eq!(lambert.graph.graph.outputs.alpha_cutoff, Some(0.5));
    assert_eq!(lambert.graph.graph.nodes.len(), 22);
    assert_eq!(
        lambert.bindings.scalars,
        vec![ScalarDefault {
            name: "p".to_owned(),
            value: 2.5,
        }]
    );
    let ramp = MaterialAsset::parse(&out.get("materials/ramp.mat").ok_or("ramp")?.bytes)?;
    assert_eq!(
        ramp.bindings.textures.iter().map(|t| t.flags).collect::<Vec<_>>(),
        vec![TEXTURE_SRGB, 0],
        "slot 0 is a gap the format keeps; the ramp is linear"
    );
    let unlit = MaterialAsset::parse(&out.get("materials/unlit.mat").ok_or("unlit")?.bytes)?;
    assert!(unlit.bindings.textures.is_empty());
    assert_eq!(unlit.color_index("col"), Some(0));
    Ok(())
}

#[test]
fn graph_rules_fail_at_their_line() -> TestResult {
    // Unknown node name in `inputs`.
    let m = TOON.replace(
        r#"inputs = ["albedo", "tint"]"#,
        r#"inputs = ["albedo", "missing"]"#,
    );
    assert_at(&m, r#"inputs = ["albedo", "missing"]"#, "unknown node `missing`")?;
    // A cycle.
    let m = TOON.replace(r#"inputs = ["uv"]"#, r#"inputs = ["rgb"]"#);
    assert_at(&m, r#"inputs = ["rgb"]"#, "cycle")?;
    // A type mismatch: vec2 times vec4, reported at the node header.
    let m = TOON.replace(r#"inputs = ["albedo", "tint"]"#, r#"inputs = ["uv", "tint"]"#);
    assert_at(&m, "[node.tinted]", "input types")?;
    // A swizzle past the input's components (z of a vec2).
    let m = TOON.replace(r#"inputs = ["tinted"]"#, r#"inputs = ["uv"]"#);
    assert_at(&m, "[node.rgb]", "out of range")?;
    // A bad swizzle letter.
    let m = TOON.replace(r#"components = "xyz""#, r#"components = "xyq""#);
    assert_at(&m, "components =", "not one of xyzw")?;
    // Parameters by name: unknown, the wrong kind, unread, a bad name, a bad default.
    let m = TOON.replace("name = \"tint\"", "name = \"tone\"");
    assert_at(&m, "name = \"tone\"", "no `[param.tone]`")?;
    let m = TOON.replace(
        "kind = \"color\"\ndefault = [1.0, 0.9, 0.8]",
        "kind = \"scalar\"\ndefault = [1.0]",
    );
    assert_at(&m, "name = \"tint\"", "is a scalar")?;
    let m = format!("{TOON}\n[param.extra]\nkind = \"scalar\"\ndefault = [1.0]\n");
    assert_at(&m, "[param.extra]", "not read by any node")?;
    let m = format!("{TOON}\n[param.Bad]\nkind = \"scalar\"\ndefault = [1.0]\n");
    assert_at(&m, "[param.Bad]", "must be 1 to 31 bytes")?;
    let m = TOON.replace("default = [1.0, 0.9, 0.8]", "default = [1.0, 0.9]");
    assert_at(&m, "default = [1.0, 0.9]", "3 or 4 numbers")?;
    let m = TOON.replace("name = \"tint\"", "index = 0");
    assert_at(&m, "index = 0", "unknown key `index`")?;
    // A texture listed past the highest slot read.
    let m = TOON.replace(
        r#"textures = ["textures/albedo.testtex"]"#,
        r#"textures = ["textures/albedo.testtex", "textures/ramp.testtex"]"#,
    );
    assert_at(&m, "textures = [", "read slots below 1 only")?;
    let m = TOON.replace("slot = 0", "slot = 2");
    assert_at(&m, "slot = 2", "texture slot 2 has no texture")?;
    // Unknown op, unknown key, wrong input count.
    let m = TOON.replace(r#"op = "uv0""#, r#"op = "uv9""#);
    assert_at(&m, r#"op = "uv9""#, "is not one of")?;
    let m = TOON.replace(r#"op = "uv0""#, "op = \"uv0\"\ncolour = 1");
    assert_at(&m, "colour = 1", "unknown key `colour`")?;
    let m = TOON.replace(r#"inputs = ["albedo", "tint"]"#, r#"inputs = ["albedo"]"#);
    assert_at(&m, r#"inputs = ["albedo"]"#, "takes 2 inputs, not 1")?;
    // A constant needs 1 to 4 values.
    let m = TOON.replace(
        "[node.tint]\nop = \"color_param\"\nname = \"tint\"",
        "[node.tint]\nop = \"constant\"\nvalue = [1, 2, 3, 4, 5]",
    );
    assert_at(&m, "value = [1, 2, 3, 4, 5]", "1 to 4 numbers")?;
    // Unknown table.
    let m = format!("{TOON}\n[nodes.extra]\nop = \"time\"\n");
    assert_at(&m, "[nodes.extra]", "unknown table")?;
    Ok(())
}

#[test]
fn output_and_setting_rules_fail_at_their_line() -> TestResult {
    // An output of the wrong type.
    let m = TOON.replace("base_color = \"rgb\"", "base_color = \"rgb\"\nalpha = \"rgb\"");
    assert_at(&m, "alpha = \"rgb\"", "output `alpha` must be an f32 node")?;
    let m = TOON.replace("base_color = \"rgb\"", "base_color = \"uv\"");
    assert_at(&m, "base_color = \"uv\"", "output `base_color`")?;
    let m = TOON.replace("base_color = \"rgb\"", "base_color = \"nothing\"");
    assert_at(&m, "base_color = \"nothing\"", "unknown node `nothing`")?;
    // Lighting, rim, outline, and cutoff ranges.
    let m = TOON.replace("bands = 3", "bands = 9");
    assert_at(&m, "bands = 9", "outside 2 to 8")?;
    let m = TOON.replace("softness = 0.2", "softness = 1.5");
    assert_at(&m, "softness = 1.5", "outside 0 to 1")?;
    let m = TOON.replace("power = 3", "power = 0");
    assert_at(&m, "power = 0", "must be > 0")?;
    let m = TOON.replace("intensity = 0.6", "intensity = -1");
    assert_at(&m, "intensity = -1", ">= 0")?;
    let m = TOON.replace("width_px = 2.0", "width_px = 0.0");
    assert_at(&m, "width_px = 0.0", "must be > 0")?;
    let m = TOON.replace("casts_shadows = true", "alpha_cutoff = 2.0");
    assert_at(&m, "alpha_cutoff = 2.0", "outside 0 to 1")?;
    let m = TOON.replace(r#"model = "toon""#, r#"model = "phong""#);
    assert_at(&m, r#"model = "phong""#, "is not one of")?;
    let m = TOON.replace(r#"["skinned", "vat"]"#, r#"["skinned", "cloth"]"#);
    assert_at(&m, "deformations =", "unknown deformation `cloth`")?;
    let m = TOON.replace("shadow_tint = [0.55, 0.5, 0.7]", "shadow_tint = [0.55, 0.5]");
    assert_at(&m, "shadow_tint =", "must hold 3 numbers")?;
    let m = TOON.replace(
        "[lighting]\nmodel = \"toon\"",
        "[lighting]\nmodel = \"toon_ramp\"\nramp_slot = 3",
    );
    let m = m
        .replace("bands = 3\n", "")
        .replace("softness = 0.2\n", "")
        .replace("shadow_tint = [0.55, 0.5, 0.7]\n", "");
    assert_at(&m, "ramp_slot = 3", "has no texture")?;
    Ok(())
}

#[test]
fn texture_references_resolve_by_source_path() -> TestResult {
    let m = TOON.replace("textures/albedo.testtex", "textures/missing.testtex");
    assert_at(&m, "textures = [", "textures/missing.testtex")?;
    let m = TOON.replace(
        r#"textures = ["textures/albedo.testtex"]"#,
        r#"textures = ["a.testtex", "b.testtex", "c.testtex", "d.testtex", "e.testtex"]"#,
    );
    assert_at(&m, "textures = [", "at most 4")?;
    // A texture path that is not a texture output (here, another material).
    let mut t = tree_with(&TOON.replace("textures/albedo.testtex", "materials/other.material.toml"));
    t.insert("materials/other.material.toml", TOON);
    let errors = cook(&t).err().ok_or("expected errors")?;
    assert!(
        errors
            .iter()
            .any(|e| e.file == PATH && e.line == line_of(TOON, "textures = [")),
        "{errors:?}"
    );
    Ok(())
}

/// With the built-in texture importer: a `*.ppm` source named by the material resolves
/// to the cooked texture's content hash.
#[test]
fn a_netpbm_texture_source_resolves_through_the_texture_importer() -> TestResult {
    let mut ppm = b"P6\n4 4\n255\n".to_vec();
    for i in 0u8..16 {
        ppm.extend_from_slice(&[i * 16, 255 - i * 16, 128]);
    }
    let material = r#"
textures = ["textures/rock.ppm"]
[lighting]
model = "lambert"
[output]
base_color = "rgb"
[node.uv]
op = "uv0"
[node.tex]
op = "texture"
slot = 0
inputs = ["uv"]
[node.rgb]
op = "swizzle"
components = "rgb"
inputs = ["tex"]
"#;
    let mut t = ContentTree::new();
    t.insert("textures/rock.ppm", ppm);
    t.insert("materials/rock.material.toml", material);
    let out = Cook::new(importers::builtin())?
        .run(&t)
        .map_err(|e| format!("{e:?}"))?;
    let texture = out.get("textures/rock.tex").ok_or("texture output")?;
    assert_eq!(texture.kind, AssetKind::Texture);
    let rock = MaterialAsset::parse(&out.get("materials/rock.mat").ok_or("material")?.bytes)?;
    assert_eq!(
        rock.bindings.textures,
        vec![TextureRef {
            hash: texture.hash,
            flags: TEXTURE_SRGB,
        }],
        "the cooked texture's hash and sRGB flag"
    );
    Ok(())
}

#[test]
fn materials_cook_deterministically() -> TestResult {
    let a = cook(&tree_with(TOON)).map_err(|e| format!("{e:?}"))?;
    let b = cook(&tree_with(TOON)).map_err(|e| format!("{e:?}"))?;
    assert_eq!(
        a.get("materials/crate.mat").map(|x| &x.bytes),
        b.get("materials/crate.mat").map(|x| &x.bytes)
    );
    assert_eq!(
        a.bundle(Domain::Presentation, 1).hash(),
        b.bundle(Domain::Presentation, 1).hash()
    );
    Ok(())
}

#[test]
fn naga_rejects_invalid_wgsl() {
    assert!(check_wgsl("fn main( {").is_err(), "syntax");
    assert!(
        check_wgsl("fn f() -> f32 { return vec2<f32>(1.0, 2.0); }").is_err(),
        "types"
    );
    assert!(check_wgsl("fn f() -> f32 { return 1.0; }").is_ok());
}
