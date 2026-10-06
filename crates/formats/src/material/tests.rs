//! Material asset tests: validation, permutations, binary round trip and rejection.

use super::*;

type TestResult = Result<(), Box<dyn std::error::Error>>;

fn graph(nodes: Vec<Node>, base: u16) -> MaterialGraph {
    MaterialGraph {
        nodes,
        outputs: SurfaceOutputs {
            base_color: NodeId(base),
            alpha: None,
            emissive: None,
            alpha_cutoff: None,
        },
        lighting: LightingModel::Lambert,
        rim: None,
        outline: None,
    }
}

fn color() -> Node {
    Node::Constant([1.0, 0.5, 0.25, 0.0], ValueType::Vec3)
}

#[test]
fn reference_materials_validate_and_type() -> TestResult {
    let refs = reference_materials()?;
    assert_eq!(refs.len(), 4);
    let toon = refs.first().ok_or("toon")?;
    assert_eq!(toon.graph.types.last(), Some(&ValueType::Vec3));
    assert_eq!(toon.graph.slots_used, [true, false, false, false]);
    let foliage = refs.get(1).ok_or("foliage")?;
    assert_eq!(
        foliage.graph.slots_used,
        [false, true, true, false],
        "ramp slot counts as used"
    );
    Ok(())
}

#[test]
fn validation_rejects_every_malformed_graph() {
    let bad = |g: MaterialGraph| g.validate().err();
    let n = NodeId;
    assert_eq!(bad(graph(vec![], 0)), Some(MaterialError::NodeCount));
    assert_eq!(
        bad(graph(vec![Node::Normalize(n(0))], 0)),
        Some(MaterialError::ForwardReference { node: 0, input: 0 })
    );
    assert_eq!(
        bad(graph(vec![Node::Uv0, color(), Node::Add(n(0), n(1))], 2)),
        Some(MaterialError::TypeMismatch { node: 2 })
    );
    assert_eq!(
        bad(graph(vec![Node::Time, Node::Dot(n(0), n(0)), color()], 2)),
        Some(MaterialError::TypeMismatch { node: 1 })
    );
    assert_eq!(
        bad(graph(vec![Node::Time, Node::Normalize(n(0)), color()], 2)),
        Some(MaterialError::TypeMismatch { node: 1 })
    );
    assert_eq!(
        bad(graph(
            vec![Node::Uv0, Node::Swizzle(n(0), [2, 0, 0, 0], 1), color()],
            2
        )),
        Some(MaterialError::OutOfRange { node: Some(1) })
    );
    assert_eq!(
        bad(graph(
            vec![
                Node::Time,
                Node::Combine([Some(n(0)), None, Some(n(0)), None]),
                color()
            ],
            2
        )),
        Some(MaterialError::OutOfRange { node: Some(1) })
    );
    assert_eq!(
        bad(graph(
            vec![Node::Time, Node::Combine([Some(n(0)), None, None, None]), color()],
            2
        )),
        Some(MaterialError::OutOfRange { node: Some(1) })
    );
    assert_eq!(
        bad(graph(vec![Node::ScalarParam(16), color()], 1)),
        Some(MaterialError::OutOfRange { node: Some(0) })
    );
    assert_eq!(
        bad(graph(vec![Node::ColorParam(8), color()], 1)),
        Some(MaterialError::OutOfRange { node: Some(0) })
    );
    assert_eq!(
        bad(graph(
            vec![Node::Uv0, Node::Texture { slot: 4, uv: n(0) }, color()],
            2
        )),
        Some(MaterialError::OutOfRange { node: Some(1) })
    );
    assert_eq!(
        bad(graph(
            vec![Node::Time, Node::Texture { slot: 0, uv: n(0) }, color()],
            2
        )),
        Some(MaterialError::TypeMismatch { node: 1 })
    );
    assert_eq!(
        bad(graph(
            vec![Node::Constant([1.0, 2.0, 0.0, 0.0], ValueType::F32)],
            0
        )),
        Some(MaterialError::OutOfRange { node: Some(0) }),
        "unused component set"
    );
    assert_eq!(
        bad(graph(
            vec![Node::Constant([f32::NAN, 0.0, 0.0, 0.0], ValueType::F32)],
            0
        )),
        Some(MaterialError::OutOfRange { node: Some(0) })
    );
}

#[test]
fn validation_rejects_bad_outputs_and_settings() {
    let bad = |g: MaterialGraph| g.validate().err();
    assert_eq!(
        bad(graph(vec![Node::Time], 0)),
        Some(MaterialError::Output("base_color"))
    );
    assert_eq!(
        bad(graph(vec![color()], 5)),
        Some(MaterialError::Output("base_color"))
    );
    let mut g = graph(vec![color()], 0);
    g.outputs.alpha = Some(NodeId(0));
    assert_eq!(bad(g), Some(MaterialError::Output("alpha")));
    let mut g = graph(vec![color()], 0);
    g.outputs.alpha_cutoff = Some(1.5);
    assert_eq!(bad(g), Some(MaterialError::Output("alpha_cutoff")));
    let mut g = graph(vec![color()], 0);
    g.lighting = LightingModel::Toon {
        bands: 9,
        softness: 0.0,
        shadow_tint: [0.0; 3],
    };
    assert_eq!(bad(g), Some(MaterialError::Setting("toon")));
    let mut g = graph(vec![color()], 0);
    g.lighting = LightingModel::ToonRamp { slot: 4 };
    assert_eq!(bad(g), Some(MaterialError::Setting("toon ramp slot")));
    let mut g = graph(vec![color()], 0);
    g.rim = Some(RimLight {
        color: [1.0; 3],
        power: 0.0,
        intensity: 1.0,
    });
    assert_eq!(bad(g), Some(MaterialError::Setting("rim")));
    let mut g = graph(vec![color()], 0);
    g.outline = Some(Outline {
        width_px: f32::NAN,
        color: [0.0; 3],
    });
    assert_eq!(bad(g), Some(MaterialError::Setting("outline")));
}

#[test]
fn permutation_keys_pack_and_reject_impossible_combinations() -> TestResult {
    let k = PermutationKey::new(Pass::Forward, true, true, false);
    assert_eq!(k.map(PermutationKey::bits), Some(0b11_0000_0000));
    assert!(
        PermutationKey::new(Pass::Shadow, true, false, false).is_none(),
        "lightmapped shadow"
    );
    for bits in 0..0x4000u32 {
        if let Some(k) = PermutationKey::from_bits(bits) {
            assert_eq!(k.bits(), bits);
            assert_eq!(
                PermutationKey::new(k.pass(), k.lightmapped(), k.alpha_test(), k.bindless())
                    .and_then(|base| base.with_deform(k.deform())),
                Some(k)
            );
        }
    }
    assert!(PermutationKey::from_bits(1 << 13).is_none(), "unknown bit");
    assert!(
        PermutationKey::from_bits(3 << 11).is_none(),
        "unknown deformation"
    );
    assert!(PermutationKey::from_bits(KEY_LIGHTMAPPED | 2).is_none());
    assert!(
        PermutationKey::from_bits(KEY_LIGHTMAPPED | (1 << 11)).is_none(),
        "lightmapped skinned"
    );
    let skinned = PermutationKey::new(Pass::Shadow, false, false, true)
        .and_then(|k| k.with_deform(Deform::Skinned))
        .ok_or("skinned key")?;
    assert_eq!(
        (skinned.pass(), skinned.deform(), skinned.bindless()),
        (Pass::Shadow, Deform::Skinned, true)
    );
    Ok(())
}

#[test]
fn canonical_permutations_follow_the_material() -> TestResult {
    let refs = reference_materials()?;
    let count = |i: usize| refs.get(i).map(|m| m.canonical_permutations().len());
    // Static: forward x lightmapped x bindless (4) + depth (2) + shadow (2) + outline (2);
    // each of skinned and vertex animation: four passes x bindless, never lightmapped (8).
    assert_eq!(count(0), Some(26), "toon: shadows, outline, both deformations");
    assert_eq!(count(1), Some(8), "foliage: shadows");
    assert_eq!(count(2), Some(6), "lambert: no shadows");
    let foliage = refs.get(1).ok_or("foliage")?;
    assert!(foliage.canonical_permutations().iter().all(|k| k.alpha_test()));
    let unlit = refs.get(3).ok_or("unlit")?;
    assert_eq!(unlit.request(Pass::Shadow, false, false), None);
    assert_eq!(unlit.request(Pass::Outline, false, false), None);
    let k = unlit.request(Pass::Depth, true, false).ok_or("depth")?;
    assert!(!k.lightmapped(), "lightmapping only applies to the forward pass");
    Ok(())
}

#[test]
fn every_requestable_permutation_is_canonical_and_vice_versa() -> TestResult {
    // The cook compiles canonical_permutations(); the renderer asks via request(). Both
    // directions must agree for every material and every request.
    for m in reference_materials()? {
        let canonical = m.canonical_permutations();
        let mut requested = Vec::new();
        for pass in Pass::ALL {
            for deform in Deform::ALL {
                for bindless in [false, true] {
                    if let Some(k) = m.request_deformed(pass, bindless, deform) {
                        assert!(canonical.contains(&k), "renderer would request {k:?}");
                        assert_eq!(k.deform(), deform);
                        requested.push(k);
                    }
                }
            }
            for lightmapped in [false, true] {
                for bindless in [false, true] {
                    if let Some(k) = m.request(pass, lightmapped, bindless) {
                        assert!(
                            canonical.contains(&k),
                            "renderer would request {k:?}, cook never emits it"
                        );
                        requested.push(k);
                    }
                }
            }
        }
        for k in &canonical {
            assert!(
                requested.contains(k),
                "cook emits {k:?}, renderer never requests it"
            );
        }
    }
    Ok(())
}

#[test]
fn binary_round_trips() -> TestResult {
    for m in reference_materials()? {
        let bytes = m.encode();
        assert_eq!(MaterialAsset::parse(&bytes)?, m);
    }
    Ok(())
}

fn put(b: &mut [u8], at: usize, v: &[u8]) {
    if let Some(s) = b.get_mut(at..at + v.len()) {
        s.copy_from_slice(v);
    }
}

#[test]
fn binary_rejects_malformed_and_non_canonical_assets() -> TestResult {
    let m = reference_materials()?.into_iter().next().ok_or("toon")?;
    let good = m.encode();
    let with = |at: usize, v: &[u8]| {
        let mut b = good.clone();
        put(&mut b, at, v);
        MaterialAsset::parse(&b).err()
    };
    assert_eq!(with(0, b"MMAX"), Some(FormatError::Magic));
    assert_eq!(with(4, &3u16.to_le_bytes()), Some(FormatError::Version(3)));
    assert_eq!(with(6, &8u16.to_le_bytes()), Some(FormatError::Flags(8)));
    // Dropping vertex animation support: the stored permutation list no longer matches.
    assert_eq!(with(6, &3u16.to_le_bytes()), Some(FormatError::Inconsistent));
    assert_eq!(with(8, &0u32.to_le_bytes()), Some(FormatError::Dimensions));
    assert_eq!(with(12, &[9]), Some(FormatError::Encoding(9)));
    assert_eq!(with(15, &[1]), Some(FormatError::Reserved), "reserved byte");
    assert_eq!(
        with(14, &[1]),
        Some(FormatError::Reserved),
        "ramp slot set on a toon material"
    );
    assert_eq!(with(32, &2u32.to_le_bytes()), Some(FormatError::Inconsistent));
    assert_eq!(with(13, &[9]), Some(FormatError::Inconsistent), "nine toon bands");
    // Flip shadows off: the stored permutation list no longer matches.
    assert_eq!(with(6, &0u16.to_le_bytes()), Some(FormatError::Inconsistent));
    // A node with a field its op does not use: the first node (uv0) gets an aux byte.
    let tables = 4 + (32 + 4) + 4 + 4 + (16 + 1 + 4);
    let nodes_at = good.len() - tables - 5 * 28;
    assert_eq!(with(nodes_at + 1, &[1]), Some(FormatError::Reserved));
    assert_eq!(with(nodes_at, &[99]), Some(FormatError::Encoding(99)));
    assert!(matches!(
        MaterialAsset::parse(good.get(..good.len() - 1).unwrap_or(&[])),
        Err(FormatError::Length { .. })
    ));
    Ok(())
}

fn with_bindings(
    edit: impl FnOnce(&mut MaterialBindings),
) -> Result<Option<String>, Box<dyn std::error::Error>> {
    let mut m = reference_materials()?.into_iter().next().ok_or("toon")?;
    edit(&mut m.bindings);
    Ok(m.bindings_error())
}

fn first_texture(b: &mut MaterialBindings) -> Option<&mut TextureRef> {
    b.textures.first_mut()
}

fn first_color(b: &mut MaterialBindings) -> Option<&mut ColorDefault> {
    b.colors.first_mut()
}

type Edit = fn(&mut MaterialBindings);

#[test]
fn binding_tables_follow_the_graph_exactly() -> TestResult {
    for m in reference_materials()? {
        assert_eq!(m.bindings_error(), None);
    }
    let cases: [(&str, Edit); 10] = [
        ("textures listed", |b| b.textures.clear()),
        ("textures listed", |b| {
            let t = b.textures.first().copied();
            b.textures.extend(t);
        }),
        ("color parameters", |b| b.colors.clear()),
        ("scalar parameters", |b| {
            b.scalars.push(ScalarDefault {
                name: "x".into(),
                value: 0.0,
            });
        }),
        ("flags", |b| {
            if let Some(t) = first_texture(b) {
                t.flags = TEXTURE_SRGB | TEXTURE_NORMAL_MAP;
            }
        }),
        ("flags", |b| {
            if let Some(t) = first_texture(b) {
                t.flags = 4;
            }
        }),
        ("name", |b| {
            if let Some(c) = first_color(b) {
                c.name = "Tint".into();
            }
        }),
        ("name", |b| {
            if let Some(c) = first_color(b) {
                c.name = String::new();
            }
        }),
        ("name", |b| {
            if let Some(c) = first_color(b) {
                c.name = "x".repeat(32);
            }
        }),
        ("finite", |b| {
            if let Some(c) = first_color(b) {
                c.value = [0.0, f32::NAN, 0.0, 1.0];
            }
        }),
    ];
    for (expected, edit) in cases {
        let error = with_bindings(edit)?.ok_or(expected)?;
        assert!(error.contains(expected), "{expected}: {error}");
    }
    // Duplicate names, in a material with two colors.
    let mut unlit = reference_materials()?.pop().ok_or("unlit")?;
    if let Some(c) = unlit.bindings.colors.get_mut(1) {
        c.name = "c0".into();
    }
    assert!(unlit.bindings_error().is_some_and(|e| e.contains("twice")));
    // Parse enforces the rule.
    let mut bad = reference_materials()?.into_iter().next().ok_or("toon")?;
    bad.bindings.colors.clear();
    assert_eq!(
        MaterialAsset::parse(&bad.encode()).err(),
        Some(FormatError::Inconsistent)
    );
    assert_eq!(unlit.color_index("glow"), Some(3));
    assert_eq!(unlit.color_index("nope"), None);
    Ok(())
}

#[test]
fn version_1_assets_parse_with_empty_bindings() -> TestResult {
    for m in reference_materials()? {
        let v1 = m.encode_v1();
        assert_eq!(v1.get(4..6), Some(&1u16.to_le_bytes()[..]));
        let parsed = MaterialAsset::parse(&v1)?;
        assert_eq!(parsed.bindings, MaterialBindings::default());
        assert_eq!(parsed.graph, m.graph);
        // A version 1 header followed by tables is not canonical.
        let mut padded = v1.clone();
        padded.extend_from_slice(&[0; 12]);
        assert!(MaterialAsset::parse(&padded).is_err());
    }
    Ok(())
}

#[test]
fn no_corruption_or_truncation_panics() -> TestResult {
    for m in reference_materials()? {
        let bytes = m.encode();
        for i in 0..bytes.len() {
            for mask in [0x01u8, 0x80, 0xff] {
                let mut b = bytes.clone();
                if let Some(x) = b.get_mut(i) {
                    *x ^= mask;
                }
                if let Ok(parsed) = MaterialAsset::parse(&b) {
                    assert_eq!(parsed.encode(), b, "anything accepted is canonical");
                }
            }
        }
        for len in 0..bytes.len() {
            assert!(MaterialAsset::parse(bytes.get(..len).unwrap_or(&[])).is_err());
        }
    }
    Ok(())
}
