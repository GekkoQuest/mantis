//! Sector sources cook to server and client copies, resolve meshes and materials by
//! source path, check shared heightfield edges bit for bit, and locate every error.

use std::sync::Arc;

use mantis_cook::importer::{CookError, Cooked, ImportContext, Importer, Source};
use mantis_cook::tree::ContentTree;
use mantis_cook::{Cook, importers};
use mantis_formats::bundle::{AssetKind, Domain};
use mantis_formats::sector::{PLACEMENT_CASTS_SHADOWS, Sector, SectorInfo, TRIGGER_SERVER_ONLY, chunk};

type TestResult = Result<(), Box<dyn std::error::Error>>;

/// Stands in for the mesh and material importers: `*.stub_mesh` and `*.stub_material`
/// cook to their bytes with the matching kind.
struct Stub;

impl Importer for Stub {
    fn name(&self) -> &'static str {
        "test.stub"
    }
    fn version(&self) -> u32 {
        1
    }
    fn phase(&self) -> u32 {
        0
    }
    fn accepts(&self, path: &str) -> bool {
        path.ends_with(".stub_mesh") || path.ends_with(".stub_material")
    }
    fn import(&self, s: &Source<'_>, _ctx: &ImportContext<'_>) -> Result<Vec<Cooked>, CookError> {
        let kind = if s.path.ends_with(".stub_mesh") {
            AssetKind::Mesh
        } else {
            AssetKind::Material
        };
        Ok(vec![Cooked {
            name: format!("{}.cooked", s.path),
            kind,
            domain: Domain::Presentation,
            bytes: s.bytes.to_vec(),
        }])
    }
}

fn cook(tree: &ContentTree) -> Result<mantis_cook::CookOutput, Vec<CookError>> {
    let mut all = importers::builtin();
    all.push(Arc::new(Stub));
    Cook::new(all).map_err(|e| vec![e])?.run(tree)
}

const SECTOR: &str = r#"
[sector]
x = 0
z = 0
size = 16.0

[ground]
cell = 4.0
flat = 1.5

[streaming]
preload_radius = 24.0

[placement.crate_01]
mesh = "meshes/crate.stub_mesh"
material = "materials/wood.stub_material"
position = [8.0, 1.5, 12.0]
yaw = 90.0
scale = 2.0
lods = [40.0, 120.0]

[hull.wall]
min = [0.0, 0.0, 0.0]
max = [4.0, 3.0, 0.5]

[trigger.secret]
id = 7
kind = 2
min = [1.0, 0.0, 1.0]
max = [2.0, 2.0, 2.0]
server_only = true
on_enter = true

[trigger.door]
id = 8
kind = 3
min = [3.0, 0.0, 1.0]
max = [4.0, 2.0, 2.0]
on_enter = true
"#;

fn tree() -> ContentTree {
    let mut t = ContentTree::new();
    t.insert("meshes/crate.stub_mesh", "mesh bytes");
    t.insert("materials/wood.stub_material", "material bytes");
    t.insert("sectors/0_0.sector.toml", SECTOR);
    t
}

#[test]
fn a_sector_cooks_to_server_and_client_copies() -> TestResult {
    let out = cook(&tree()).map_err(|e| format!("{e:?}"))?;
    let server = Sector::parse(&out.get("sectors/0_0.server.sector").ok_or("server copy")?.bytes)?;
    let client_asset = out.get("sectors/0_0.sector").ok_or("client copy")?;
    assert_eq!(
        client_asset.domain,
        Domain::Gameplay,
        "the handshake covers the client copy"
    );
    assert_eq!(
        out.get("sectors/0_0.server.sector").map(|a| a.domain),
        Some(Domain::Server)
    );
    let client = Sector::parse(&client_asset.bytes)?;
    assert_eq!(client, server.for_client());
    let server_triggers = server.triggers.as_ref().ok_or("triggers")?;
    assert!(server_triggers.iter().any(|t| t.flags & TRIGGER_SERVER_ONLY != 0));
    assert!(
        client
            .triggers
            .as_ref()
            .ok_or("triggers")?
            .iter()
            .all(|t| t.flags & TRIGGER_SERVER_ONLY == 0)
    );
    assert_eq!(
        client.ground, server.ground,
        "the client keeps the ground for prediction"
    );
    assert_eq!(client.hulls, server.hulls, "and the collision");
    let ground = server.ground.as_ref().ok_or("ground")?;
    assert_eq!(
        (ground.width, ground.depth, ground.heights.first().copied()),
        (5, 5, Some(1.5))
    );
    assert_eq!(server.placements, None, "placements are visual (decision 0020)");
    let visual_asset = out.get("sectors/0_0.visual").ok_or("visual copy")?;
    assert_eq!(visual_asset.domain, Domain::Presentation);
    let visual = Sector::parse(&visual_asset.bytes)?;
    assert_eq!(visual.chunk_ids(), vec![chunk::SECT, chunk::PLAC]);
    assert_eq!(visual.info, server.info);
    let p = visual
        .placements
        .as_ref()
        .and_then(|p| p.first())
        .ok_or("placement")?;
    assert_eq!(
        p.mesh,
        out.get("meshes/crate.stub_mesh.cooked").ok_or("mesh")?.hash
    );
    assert_eq!(
        p.material,
        out.get("materials/wood.stub_material.cooked")
            .ok_or("material")?
            .hash
    );
    assert_eq!(p.flags, PLACEMENT_CASTS_SHADOWS);
    assert_eq!(p.lod_count, 2);
    // Yaw 90 turns +Z toward +X: the placement's z axis is world +X, scaled by 2.
    let [_, _, _, _, _, _, zx, zy, zz, tx, ty, tz] = p.transform;
    assert!(
        (zx - 2.0).abs() < 1e-5 && zy.abs() < 1e-5 && zz.abs() < 1e-5,
        "{:?}",
        p.transform
    );
    assert_eq!((tx, ty, tz), (8.0, 1.5, 12.0));
    Ok(())
}

#[test]
fn shared_edges_must_match_bit_for_bit() -> TestResult {
    let mut t = tree();
    let neighbor = SECTOR
        .replace("x = 0", "x = 1")
        .replace("flat = 1.5", "flat = 1.25");
    t.insert("sectors/1_0.sector.toml", neighbor);
    let errors = cook(&t).err().ok_or("a mismatched edge must fail")?;
    assert!(
        errors
            .iter()
            .any(|e| e.file == "sectors/0_0.sector.toml" && e.message.contains("shared edge")),
        "{errors:?}"
    );
    let mut t = tree();
    t.insert("sectors/1_0.sector.toml", SECTOR.replace("x = 0", "x = 1"));
    assert!(cook(&t).is_ok(), "matching neighbors cook");
    Ok(())
}

#[test]
fn heightmaps_feed_the_ground() -> TestResult {
    let mut t = tree();
    let mut pgm = b"P5\n5 5\n255\n".to_vec();
    pgm.extend((0..25u8).map(|i| i * 10));
    t.insert("terrain/0_0.pgm", pgm);
    t.insert(
        "sectors/0_0.sector.toml",
        SECTOR.replace(
            "flat = 1.5",
            "heightmap = \"terrain/0_0.pgm\"\nheight_scale = 0.5",
        ),
    );
    let out = cook(&t).map_err(|e| format!("{e:?}"))?;
    let s = Sector::parse(&out.get("sectors/0_0.sector").ok_or("sector")?.bytes)?;
    let g = s.ground.ok_or("ground")?;
    assert_eq!(g.heights.get(24).copied(), Some(120.0));
    assert!(
        out.get("terrain/0_0.tex").is_none(),
        "a heightmap is the sector's input, never cooked as a texture"
    );
    Ok(())
}

#[test]
fn errors_name_the_line() -> TestResult {
    let cases: [(&str, &str, usize); 6] = [
        ("x = 0", "x = 3", 3),                                      // wrong file name
        ("cell = 4.0", "cell = 5.0", 8),                            // not a whole number of cells
        ("scale = 2.0", "scale = 2.0\nsize = 1.0", 20),             // unknown key
        ("meshes/crate.stub_mesh", "meshes/missing.stub_mesh", 15), // unresolved mesh
        ("lods = [40.0, 120.0]", "lods = [120.0, 40.0]", 20),       // decreasing lods
        ("max = [4.0, 3.0, 0.5]", "max = [-4.0, 3.0, 0.5]", 24),    // inverted box
    ];
    for (from, to, line) in cases {
        let mut t = tree();
        t.insert("sectors/0_0.sector.toml", SECTOR.replacen(from, to, 1));
        let errors = cook(&t).err().ok_or(format!("`{to}` must fail"))?;
        assert!(
            errors
                .iter()
                .any(|e| e.file == "sectors/0_0.sector.toml" && e.line == line),
            "`{to}`: expected line {line}, got {errors:?}"
        );
    }
    Ok(())
}

#[test]
fn cooking_is_deterministic() -> TestResult {
    let a = cook(&tree()).map_err(|e| format!("{e:?}"))?;
    let b = cook(&tree()).map_err(|e| format!("{e:?}"))?;
    assert_eq!(
        a.bundle(Domain::Gameplay, 1).hash(),
        b.bundle(Domain::Gameplay, 1).hash()
    );
    assert_eq!(
        a.bundle(Domain::Server, 1).hash(),
        b.bundle(Domain::Server, 1).hash()
    );
    Ok(())
}

#[test]
fn a_drawn_ground_cooks_to_an_upward_mesh_placed_at_the_sector_origin() -> TestResult {
    let mut content = tree();
    let mut pgm = b"P5
5 5
255
"
    .to_vec();
    pgm.extend((0..25u8).map(|i| (i % 5) * 20));
    content.insert("terrain/1_2.pgm", pgm);
    let source = SECTOR
        .replace(
            "x = 0
z = 0",
            "x = 1
z = 2",
        )
        .replace(
            "flat = 1.5",
            "heightmap = \"terrain/1_2.pgm\"
height_scale = 0.1
material = \"materials/wood.stub_material\"",
        );
    content.insert("sectors/1_2.sector.toml", source);
    let out = cook(&content).map_err(|e| format!("{e:?}"))?;
    let asset = out.get("sectors/1_2.ground.mesh").ok_or("ground mesh")?;
    assert_eq!(
        (asset.kind, asset.domain),
        (AssetKind::Mesh, Domain::Presentation)
    );
    let mesh = mantis_formats::mesh::MeshAsset::parse(&asset.bytes)?;
    assert_eq!((mesh.vertices.len(), mesh.indices.len()), (25, 16 * 6));
    let position = |index: u32| {
        mesh.vertices
            .get(index as usize)
            .map(|v| v.position)
            .ok_or("index")
    };
    for tri in mesh.indices.as_chunks::<3>().0 {
        let [first, second, third] = *tri;
        let (origin, edge1, edge2) = (position(first)?, position(second)?, position(third)?);
        let u = [edge1[0] - origin[0], edge1[2] - origin[2]];
        let v = [edge2[0] - origin[0], edge2[2] - origin[2]];
        // The y component of (b - a) x (c - a).
        assert!(u[1] * v[0] - u[0] * v[1] > 0.0, "{tri:?} faces up");
    }
    // The slope rises along +x by 2 m per 4 m cell: the normal leans toward -x.
    let normal = mesh.vertices.get(12).ok_or("vertex")?.normal;
    assert!(
        normal[0] < -0.4 && normal[1] > 0.8 && normal[2].abs() < 1e-6,
        "{normal:?}"
    );
    assert_eq!(mesh.vertices.get(24).map(|v| v.uv1), Some([1.0, 1.0]));
    let sector = Sector::parse(&out.get("sectors/1_2.visual").ok_or("visual")?.bytes)?;
    let placements = sector.placements.ok_or("placements")?;
    let ground = placements.first().ok_or("ground placement")?;
    assert_eq!(ground.mesh, asset.hash);
    assert_eq!(
        ground.material,
        out.get("materials/wood.stub_material.cooked")
            .ok_or("material")?
            .hash
    );
    assert_eq!(ground.transform.get(9..), Some(&[16.0, 0.0, 32.0][..]));
    assert_eq!(placements.len(), 2, "the ground plus the crate");
    // Without a material the ground stays collision only.
    let plain = cook(&tree()).map_err(|e| format!("{e:?}"))?;
    assert!(plain.get("sectors/0_0.ground.mesh").is_none());
    Ok(())
}

/// A package importer that cooks `*.rogue` sources to sector containers in a chosen
/// domain, to prove the pipeline enforces decision 0020 for every importer.
struct Rogue;

impl Importer for Rogue {
    fn name(&self) -> &'static str {
        "test.rogue"
    }
    fn version(&self) -> u32 {
        1
    }
    fn phase(&self) -> u32 {
        40
    }
    fn accepts(&self, path: &str) -> bool {
        std::path::Path::new(path)
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("rogue"))
    }
    fn import(&self, s: &Source<'_>, _ctx: &ImportContext<'_>) -> Result<Vec<Cooked>, CookError> {
        let text = s.text()?;
        let at = |n: i32| SectorInfo {
            sector_x: n,
            sector_z: 0,
            sector_size: 16.0,
            content_version: 0,
        };
        let empty = Sector {
            info: at(0),
            ground: None,
            hulls: None,
            triggers: None,
            placements: None,
            lightmaps: None,
            probe_volume: None,
            streaming: None,
        };
        let (sector, domain) = match text.trim() {
            // Visual chunks in the gameplay bundle.
            "lightmaps_in_gameplay" => (
                Sector {
                    info: at(5),
                    lightmaps: Some(Vec::new()),
                    ..empty
                },
                Domain::Gameplay,
            ),
            // A visual container with no gameplay sector at its coordinates.
            _ => (
                Sector {
                    info: at(9),
                    placements: Some(Vec::new()),
                    ..empty
                },
                Domain::Presentation,
            ),
        };
        Ok(vec![Cooked {
            name: format!("{}.cooked", s.path),
            kind: AssetKind::Sector,
            domain,
            bytes: sector.encode(),
        }])
    }
}

#[test]
fn the_pipeline_enforces_sector_domains_by_chunk_id() -> TestResult {
    for (body, expected) in [
        ("lightmaps_in_gameplay", "chunk `LMAP`"),
        ("orphan_visual", "has no gameplay sector"),
    ] {
        let mut t = tree();
        t.insert("extra/x.rogue", body);
        let mut all = importers::builtin();
        all.push(Arc::new(Stub));
        all.push(Arc::new(Rogue));
        let errors = Cook::new(all)
            .map_err(|e| format!("{e:?}"))?
            .run(&t)
            .err()
            .ok_or("must fail")?;
        assert!(
            errors
                .iter()
                .any(|e| e.file == "extra/x.rogue" && e.message.contains(expected)),
            "{body}: {errors:?}"
        );
    }
    Ok(())
}

#[test]
fn placements_name_a_mesh_per_level_of_detail() -> TestResult {
    let mut t = tree();
    t.insert("meshes/crate_mid.stub_mesh", "mid mesh bytes");
    t.insert("meshes/crate_far.stub_mesh", "far mesh bytes");
    let with = SECTOR.replace(
        "lods = [40.0, 120.0]",
        "lods = [40.0, 120.0, 300.0]\nlod_meshes = [\"meshes/crate_mid.stub_mesh\", \"meshes/crate_far.stub_mesh\"]",
    );
    t.insert("sectors/0_0.sector.toml", with);
    let out = cook(&t).map_err(|e| format!("{e:?}"))?;
    let visual = Sector::parse(&out.get("sectors/0_0.visual").ok_or("visual")?.bytes)?;
    let p = visual
        .placements
        .as_ref()
        .and_then(|v| v.first())
        .ok_or("placement")?;
    let hash = |name: &str| out.get(name).map(|a| a.hash);
    assert_eq!(p.mesh_at(0), hash("meshes/crate.stub_mesh.cooked"));
    assert_eq!(p.mesh_at(1), hash("meshes/crate_mid.stub_mesh.cooked"));
    assert_eq!(p.mesh_at(2), hash("meshes/crate_far.stub_mesh.cooked"));
    assert_eq!(p.mesh_at(3), None);
    // Without `lod_meshes` every level draws the base mesh.
    let plain = cook(&tree()).map_err(|e| format!("{e:?}"))?;
    let visual = Sector::parse(&plain.get("sectors/0_0.visual").ok_or("visual")?.bytes)?;
    let p = visual
        .placements
        .as_ref()
        .and_then(|v| v.first())
        .ok_or("placement")?;
    assert_eq!(p.mesh_at(1), Some(p.mesh));
    // A count that does not match the levels is an error at its line.
    let mut bad = tree();
    let text = SECTOR.replace(
        "lods = [40.0, 120.0]",
        "lods = [40.0, 120.0]\nlod_meshes = [\"meshes/crate.stub_mesh\", \"meshes/crate.stub_mesh\"]",
    );
    let line = text
        .lines()
        .position(|l| l.starts_with("lod_meshes"))
        .map_or(0, |i| i + 1);
    bad.insert("sectors/0_0.sector.toml", text);
    let errors = cook(&bad).err().ok_or("must fail")?;
    assert!(
        errors
            .iter()
            .any(|e| e.line == line && e.message.contains("levels 1 to 1")),
        "{errors:?}"
    );
    Ok(())
}
