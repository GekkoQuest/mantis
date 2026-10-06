//! Every permutation of every reference material is valid WGSL (parsed and validated by
//! naga, without a GPU), has the entry points its pass needs, and nothing else compiles.

use mantis_formats::material::{Deform, Pass, PermutationKey, reference_materials};
use mantis_shadergen::{Permutation, ShaderError, compile, compile_all};

type TestResult = Result<(), Box<dyn std::error::Error>>;

fn validate(p: &Permutation) -> Result<naga::Module, String> {
    let module = naga::front::wgsl::parse_str(&p.source).map_err(|e| e.emit_to_string(&p.source))?;
    naga::valid::Validator::new(
        naga::valid::ValidationFlags::all(),
        naga::valid::Capabilities::all(),
    )
    .validate(&module)
    .map_err(|e| e.emit_to_string(&p.source))?;
    Ok(module)
}

fn has_entry(module: &naga::Module, name: &str, stage: naga::ShaderStage) -> bool {
    module
        .entry_points
        .iter()
        .any(|e| e.name == name && e.stage == stage)
}

#[test]
fn every_canonical_permutation_is_valid_wgsl_with_the_right_entry_points() -> TestResult {
    let mut checked = 0;
    for asset in reference_materials()? {
        for p in compile_all(&asset)? {
            let module = validate(&p).map_err(|e| format!("{:?}: {e}", p.key))?;
            assert!(
                has_entry(&module, p.vertex_entry, naga::ShaderStage::Vertex),
                "{:?}",
                p.key
            );
            if let Some(fs) = p.fragment_entry {
                assert!(has_entry(&module, fs, naga::ShaderStage::Fragment), "{:?}", p.key);
            }
            match p.key.pass() {
                Pass::Forward | Pass::Outline => assert!(p.fragment_entry.is_some()),
                Pass::Depth => assert_eq!(
                    p.fragment_entry,
                    Some("fs_velocity"),
                    "the prepass writes velocity"
                ),
                Pass::Shadow => assert_eq!(p.fragment_entry.is_some(), p.key.alpha_test()),
            }
            // The vertex input follows the deformation: only skinned permutations take the
            // second (joints and weights) stream, only vertex animation reads frames.
            let vs = module
                .entry_points
                .iter()
                .find(|e| e.name == p.vertex_entry)
                .ok_or("vertex entry")?;
            assert_eq!(
                vs.function.arguments.len(),
                3,
                "vertex input, vertex index, instance index"
            );
            let skinned_input = p.source.contains("@location(4) joints");
            assert_eq!(skinned_input, p.key.deform() == Deform::Skinned, "{:?}", p.key);
            assert_eq!(
                p.source.contains("bitcast<f32>(d.z)"),
                p.key.deform() == Deform::Vat,
                "{:?}",
                p.key
            );
            checked += 1;
        }
    }
    // Toon: 10 static + 8 skinned + 8 vertex animation; foliage 8, lambert 6, unlit 6.
    assert_eq!(checked, 26 + 8 + 6 + 6, "every reference permutation compiled");
    Ok(())
}

#[test]
fn only_canonical_permutations_compile() -> TestResult {
    let refs = reference_materials()?;
    let lambert = refs.get(2).ok_or("lambert")?;
    let shadow = PermutationKey::new(Pass::Shadow, false, false, false).ok_or("key")?;
    assert_eq!(
        compile(lambert, shadow).err(),
        Some(ShaderError::NotAPermutation(shadow)),
        "lambert casts no shadows"
    );
    let alpha_forward = PermutationKey::new(Pass::Forward, false, true, false).ok_or("key")?;
    assert!(
        compile(lambert, alpha_forward).is_err(),
        "lambert has no alpha test"
    );
    Ok(())
}

#[test]
fn generated_code_reflects_the_graph() -> TestResult {
    let refs = reference_materials()?;
    let toon = refs.first().ok_or("toon")?;
    let fwd = compile(toon, toon.request(Pass::Forward, false, false).ok_or("key")?)?;
    assert!(
        fwd.source
            .contains("textureSample(material_texture_0, material_sampler, n0)")
    );
    assert!(fwd.source.contains("let bands = 3.0;"));
    assert!(
        fwd.source.contains("fn shade("),
        "forward links the lighting library"
    );
    let bindless = compile(toon, toon.request(Pass::Forward, false, true).ok_or("key")?)?;
    assert!(
        bindless
            .source
            .contains("material_textures[material.texture_index[0]]")
    );
    let outline = compile(toon, toon.request(Pass::Outline, false, false).ok_or("key")?)?;
    assert!(outline.source.contains("const OUTLINE_WIDTH_PX: f32 = 2.0;"));
    assert!(
        !outline.source.contains("fn shade("),
        "only the forward pass links lighting"
    );
    let lightmapped = compile(toon, toon.request(Pass::Forward, true, false).ok_or("key")?)?;
    assert!(lightmapped.source.contains("const LIGHTMAPPED: bool = true;"));
    Ok(())
}

#[test]
fn the_workspace_builds_exactly_one_naga() -> TestResult {
    // wgpu validates shaders with its own naga; the tests here use the workspace pin. If a
    // wgpu upgrade moves its naga, the lock file gains a second version and this fails,
    // instead of two copies silently disagreeing.
    let lock = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../Cargo.lock"))?;
    let versions: Vec<&str> = lock
        .split("[[package]]")
        .filter(|block| block.lines().any(|l| l.trim() == "name = \"naga\""))
        .filter_map(|block| block.lines().find_map(|l| l.trim().strip_prefix("version = ")))
        .collect();
    assert_eq!(versions.len(), 1, "naga versions in Cargo.lock: {versions:?}");
    Ok(())
}
