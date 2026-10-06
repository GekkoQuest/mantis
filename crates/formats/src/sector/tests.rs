//! Sector container tests: round trip, every rejection rule, and the client view.

use super::*;

type TestResult = Result<(), Box<dyn std::error::Error>>;

fn h(b: u8) -> ContentHash {
    ContentHash::from_bytes([b; 32])
}

fn full() -> Sector {
    Sector {
        info: SectorInfo {
            sector_x: -3,
            sector_z: 7,
            sector_size: 64.0,
            content_version: 5,
        },
        ground: Some(GroundGrid {
            origin_x: -192.0,
            origin_z: 448.0,
            cell_size: 32.0,
            width: 3,
            depth: 3,
            heights: vec![0.0, 1.0, 2.0, 0.5, 1.5, 2.5, 1.0, 2.0, 3.0],
        }),
        hulls: Some(vec![ConvexHull {
            aabb_min: [0.0, 0.0, 0.0],
            aabb_max: [1.0, 1.0, 1.0],
            flags: HULL_WALKABLE_TOP | HULL_BLOCKS_MOVEMENT,
            planes: vec![
                [1.0, 0.0, 0.0, 1.0],
                [-1.0, 0.0, 0.0, 0.0],
                [0.0, 1.0, 0.0, 1.0],
                [0.0, -1.0, 0.0, 0.0],
                [0.0, 0.0, 1.0, 1.0],
                [0.0, 0.0, -1.0, 0.0],
            ],
        }]),
        triggers: Some(vec![
            Trigger {
                id: 1,
                kind: 9,
                flags: TRIGGER_ON_ENTER,
                aabb_min: [0.0; 3],
                aabb_max: [2.0; 3],
            },
            Trigger {
                id: 2,
                kind: 3,
                flags: TRIGGER_SERVER_ONLY | TRIGGER_ON_EXIT,
                aabb_min: [1.0; 3],
                aabb_max: [4.0; 3],
            },
        ]),
        placements: Some(vec![
            Placement {
                mesh: h(1),
                lod_meshes: [h(3), ContentHash::ZERO, ContentHash::ZERO],
                material: h(40),
                transform: [1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0, 5.0, 0.0, -2.0],
                lod_count: 2,
                lod_ranges: [40.0, 120.0, 0.0, 0.0],
                flags: PLACEMENT_LIGHTMAPPED | PLACEMENT_CASTS_SHADOWS,
                lightmap: h(9),
                uv_scale: [0.25, 0.25],
                uv_offset: [0.5, 0.0],
            },
            Placement {
                mesh: h(2),
                lod_meshes: [ContentHash::ZERO; 3],
                material: h(40),
                transform: [0.0, 0.0, -2.0, 0.0, 2.0, 0.0, 2.0, 0.0, 0.0, 0.0, 1.0, 0.0],
                lod_count: 1,
                lod_ranges: [300.0, 0.0, 0.0, 0.0],
                flags: 0,
                lightmap: ContentHash::ZERO,
                uv_scale: [0.0; 2],
                uv_offset: [0.0; 2],
            },
        ]),
        lightmaps: Some(vec![h(9)]),
        probe_volume: Some(h(7)),
        streaming: Some(StreamingHints {
            priority_bias: 0.5,
            preload_radius: 96.0,
            lod_distance_scale: 1.0,
        }),
    }
}

#[test]
fn round_trips_every_chunk() -> TestResult {
    let s = full();
    let bytes = s.encode();
    assert_eq!(bytes.len() % 4, 0);
    assert_eq!(Sector::parse(&bytes)?, s);
    // The minimal container: SECT only.
    let minimal = Sector {
        info: s.info,
        ground: None,
        hulls: None,
        triggers: None,
        placements: None,
        lightmaps: None,
        probe_volume: None,
        streaming: None,
    };
    assert_eq!(Sector::parse(&minimal.encode())?, minimal);
    Ok(())
}

#[test]
fn client_view_drops_server_only_triggers() -> TestResult {
    let client = full().for_client();
    let ids: Vec<u32> = client
        .triggers
        .as_deref()
        .unwrap_or(&[])
        .iter()
        .map(|t| t.id)
        .collect();
    assert_eq!(ids, vec![1]);
    assert_eq!(Sector::parse(&client.encode())?, client);
    Ok(())
}

/// Encodes `s`, applies `edit` to the bytes, parses.
fn mutated(s: &Sector, edit: impl FnOnce(&mut Vec<u8>)) -> Result<Sector, FormatError> {
    let mut b = s.encode();
    edit(&mut b);
    Sector::parse(&b)
}

fn put(b: &mut [u8], at: usize, v: &[u8]) {
    if let Some(s) = b.get_mut(at..at + v.len()) {
        s.copy_from_slice(v);
    }
}

#[test]
fn container_rules() {
    let s = full();
    assert_eq!(
        mutated(&s, |b| put(b, 0, b"MSEX")).err(),
        Some(FormatError::Magic)
    );
    assert_eq!(
        mutated(&s, |b| put(b, 4, &2u16.to_le_bytes())).err(),
        Some(FormatError::Version(2))
    );
    assert_eq!(
        mutated(&s, |b| put(b, 6, &1u16.to_le_bytes())).err(),
        Some(FormatError::Flags(1))
    );
    assert_eq!(
        mutated(&s, |b| put(b, 8, &0u32.to_le_bytes())).err(),
        Some(FormatError::Dimensions)
    );
    assert_eq!(
        mutated(&s, |b| put(b, 12, &1u32.to_le_bytes())).err(),
        Some(FormatError::Reserved)
    );
    // First chunk header starts at 16: id, version, flags, length.
    assert_eq!(
        mutated(&s, |b| put(b, 16, b"ZZZZ")).err(),
        Some(FormatError::UnknownChunk(*b"ZZZZ"))
    );
    assert_eq!(
        mutated(&s, |b| put(b, 20, &2u16.to_le_bytes())).err(),
        Some(FormatError::Version(2))
    );
    assert_eq!(
        mutated(&s, |b| put(b, 22, &1u16.to_le_bytes())).err(),
        Some(FormatError::Flags(1))
    );
    assert!(matches!(
        mutated(&s, |b| put(b, 24, &4096u32.to_le_bytes())),
        Err(FormatError::Length { .. })
    ));
    assert!(
        matches!(mutated(&s, |b| b.push(0)), Err(FormatError::Length { .. })),
        "trailing bytes"
    );
    assert!(matches!(
        mutated(&s, |b| b.truncate(b.len() - 4)),
        Err(FormatError::Length { .. })
    ));
    // Chunk count claims one more chunk than present.
    assert!(matches!(
        mutated(&s, |b| put(b, 8, &9u32.to_le_bytes())),
        Err(FormatError::Length { .. })
    ));
}

#[test]
fn chunk_presence_rules() {
    let s = full();
    // A container whose only chunk is PRBV has no SECT.
    let mut no_sect = Writer::new();
    no_sect.bytes(&MAGIC);
    no_sect.u16(1);
    no_sect.u16(0);
    no_sect.u32(1);
    no_sect.u32(0);
    no_sect.bytes(&chunk::PRBV);
    no_sect.u16(1);
    no_sect.u16(0);
    no_sect.u32(32);
    no_sect.bytes(&[7; 32]);
    assert_eq!(
        Sector::parse(&no_sect.into_bytes()).err(),
        Some(FormatError::MissingChunk(chunk::SECT))
    );
    // Appending a copy of the first chunk (SECT: 12-byte header, 16-byte payload) and
    // bumping the count duplicates SECT.
    let bytes = s.encode();
    let mut dup = bytes.clone();
    let sect_chunk: Vec<u8> = bytes.get(16..16 + 12 + 16).unwrap_or(&[]).to_vec();
    dup.extend_from_slice(&sect_chunk);
    let count = u32::try_from(chunk::ALL.len()).unwrap_or(0) + 1;
    put(&mut dup, 8, &count.to_le_bytes());
    assert_eq!(
        Sector::parse(&dup).err(),
        Some(FormatError::DuplicateChunk(chunk::SECT))
    );
}

#[test]
fn ground_rules() {
    let bad = |edit: fn(&mut GroundGrid)| {
        let mut s = full();
        if let Some(g) = &mut s.ground {
            edit(g);
        }
        Sector::parse(&s.encode()).err()
    };
    assert_eq!(bad(|g| g.cell_size = 0.0), Some(FormatError::Geometry));
    assert_eq!(
        bad(|g| g.cell_size = 16.0),
        Some(FormatError::Inconsistent),
        "size disagrees with SECT"
    );
    assert_eq!(bad(|g| g.width = 1), Some(FormatError::Dimensions));
    assert!(
        matches!(bad(|g| g.width = 4), Some(FormatError::Length { .. })),
        "sample count disagrees"
    );
    assert_eq!(
        bad(|g| if let Some(x) = g.heights.get_mut(4) {
            *x = f32::NAN;
        }),
        Some(FormatError::NonFinite)
    );
    assert_eq!(bad(|g| g.origin_x = f32::INFINITY), Some(FormatError::NonFinite));
}

#[test]
fn hull_and_trigger_rules() {
    let hull = |edit: fn(&mut ConvexHull)| {
        let mut s = full();
        if let Some(h) = s.hulls.as_mut().and_then(|v| v.first_mut()) {
            edit(h);
        }
        Sector::parse(&s.encode()).err()
    };
    assert_eq!(hull(|h| h.flags = 8), Some(FormatError::Flags(8)));
    assert_eq!(
        hull(|h| h.aabb_min = [2.0, 0.0, 0.0]),
        Some(FormatError::Geometry)
    );
    assert_eq!(hull(|h| h.planes.truncate(3)), Some(FormatError::Dimensions));
    assert_eq!(
        hull(|h| if let Some(x) = h.planes.first_mut() {
            *x = [0.9, 0.0, 0.0, 1.0];
        }),
        Some(FormatError::Geometry),
        "not unit"
    );
    let trig = |edit: fn(&mut Vec<Trigger>)| {
        let mut s = full();
        if let Some(t) = s.triggers.as_mut() {
            edit(t);
        }
        Sector::parse(&s.encode()).err()
    };
    assert_eq!(
        trig(|t| if let Some(x) = t.get_mut(1) {
            x.id = 1;
        }),
        Some(FormatError::DuplicateId(1))
    );
    assert_eq!(
        trig(|t| if let Some(x) = t.first_mut() {
            x.flags = 16;
        }),
        Some(FormatError::Flags(16))
    );
    assert_eq!(
        trig(|t| if let Some(x) = t.first_mut() {
            x.aabb_max = [-1.0; 3];
        }),
        Some(FormatError::Geometry)
    );
}

#[test]
fn placements_must_be_similarities() {
    let place = |edit: fn(&mut Sector)| {
        let mut s = full();
        edit(&mut s);
        Sector::parse(&s.encode()).err()
    };
    // Non-uniform scale and shear fail closed (normals would be wrong).
    assert_eq!(
        place(
            |s| if let Some(p) = s.placements.as_mut().and_then(|v| v.first_mut()) {
                p.transform = [1.0, 0.0, 0.0, 0.0, 2.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0];
            }
        ),
        Some(FormatError::Geometry),
        "non-uniform scale"
    );
    assert_eq!(
        place(
            |s| if let Some(p) = s.placements.as_mut().and_then(|v| v.first_mut()) {
                p.transform = [1.0, 0.0, 0.0, 0.5, 1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0];
            }
        ),
        Some(FormatError::Geometry),
        "shear"
    );
    assert!(is_similarity([
        [0.0, 0.0, -2.0],
        [0.0, 2.0, 0.0],
        [2.0, 0.0, 0.0]
    ]));
    assert!(
        is_similarity([[-1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]]),
        "mirror"
    );
}

#[test]
#[allow(clippy::too_many_lines)] // One table of placement rule cases.
fn placement_rules() {
    let place = |edit: fn(&mut Sector)| {
        let mut s = full();
        edit(&mut s);
        Sector::parse(&s.encode()).err()
    };
    assert_eq!(
        place(
            |s| if let Some(p) = s.placements.as_mut().and_then(|v| v.first_mut()) {
                p.transform = [0.0; 12];
            }
        ),
        Some(FormatError::Geometry)
    );
    assert_eq!(
        place(
            |s| if let Some(p) = s.placements.as_mut().and_then(|v| v.first_mut()) {
                p.lod_count = 0;
            }
        ),
        Some(FormatError::Dimensions)
    );
    assert_eq!(
        place(
            |s| if let Some(p) = s.placements.as_mut().and_then(|v| v.first_mut()) {
                p.lod_ranges = [40.0, 30.0, 0.0, 0.0];
            }
        ),
        Some(FormatError::Geometry)
    );
    assert_eq!(
        place(
            |s| if let Some(p) = s.placements.as_mut().and_then(|v| v.first_mut()) {
                p.lod_ranges = [40.0, 120.0, 0.0, 1.0];
            }
        ),
        Some(FormatError::Reserved)
    );
    assert_eq!(
        place(
            |s| if let Some(p) = s.placements.as_mut().and_then(|v| v.first_mut()) {
                p.flags = 4;
            }
        ),
        Some(FormatError::Flags(4))
    );
    assert_eq!(
        place(
            |s| if let Some(p) = s.placements.as_mut().and_then(|v| v.first_mut()) {
                p.lightmap = ContentHash::ZERO;
            }
        ),
        Some(FormatError::Inconsistent)
    );
    assert_eq!(
        place(
            |s| if let Some(p) = s.placements.as_mut().and_then(|v| v.get_mut(1)) {
                p.uv_scale = [1.0, 1.0];
            }
        ),
        Some(FormatError::Inconsistent)
    );
    // Level meshes: one per level below the count, none above.
    assert_eq!(
        place(
            |s| if let Some(p) = s.placements.as_mut().and_then(|v| v.first_mut()) {
                p.lod_meshes = [ContentHash::ZERO; 3];
            }
        ),
        Some(FormatError::Inconsistent),
        "level 1 has no mesh"
    );
    assert_eq!(
        place(
            |s| if let Some(p) = s.placements.as_mut().and_then(|v| v.get_mut(1)) {
                p.lod_meshes = [h(5), ContentHash::ZERO, ContentHash::ZERO];
            }
        ),
        Some(FormatError::Inconsistent),
        "a mesh past the level count"
    );
    let s = full();
    let p = s.placements.as_ref().and_then(|v| v.first()).ok_or("placement");
    assert_eq!(
        p.map(|p| (p.mesh_at(0), p.mesh_at(1), p.mesh_at(2), p.meshes().count())),
        Ok((Some(h(1)), Some(h(3)), None, 2))
    );
    assert_eq!(
        place(|s| s.lightmaps = Some(vec![h(8)])),
        Some(FormatError::Inconsistent),
        "atlas not listed"
    );
    assert_eq!(
        place(|s| s.lightmaps = Some(vec![h(9), h(9)])),
        Some(FormatError::Inconsistent),
        "duplicate atlas"
    );
    assert_eq!(
        place(|s| s.streaming = Some(StreamingHints {
            priority_bias: 0.0,
            preload_radius: -1.0,
            lod_distance_scale: 1.0
        })),
        Some(FormatError::Geometry)
    );
}

#[test]
fn no_corruption_or_truncation_panics() {
    // Every single-byte corruption and every truncation of a full container either parses
    // (to a well-formed sector) or is rejected; nothing panics or reads out of bounds.
    let bytes = full().encode();
    for i in 0..bytes.len() {
        for mask in [0x01u8, 0x80, 0xff] {
            let mut b = bytes.clone();
            if let Some(x) = b.get_mut(i) {
                *x ^= mask;
            }
            if let Ok(s) = Sector::parse(&b) {
                assert!(Sector::parse(&s.encode()).is_ok());
            }
        }
    }
    for len in 0..bytes.len() {
        assert!(
            Sector::parse(bytes.get(..len).unwrap_or(&[])).is_err(),
            "truncated to {len}"
        );
    }
}

#[test]
fn the_domain_split_partitions_the_chunks() -> TestResult {
    let s = full();
    let gameplay = Sector::parse(&s.gameplay().encode())?;
    let visual = Sector::parse(&s.visual().encode())?;
    assert_eq!(gameplay.chunk_ids(), chunk::GAMEPLAY.to_vec());
    assert_eq!(visual.chunk_ids(), chunk::VISUAL.to_vec());
    assert_eq!(gameplay.info, visual.info, "joined by SECT");
    assert_eq!(gameplay.placements, None);
    assert_eq!(visual.placements, s.placements);
    assert_eq!(visual.ground, None);
    let mut every: Vec<[u8; 4]> = chunk::GAMEPLAY
        .iter()
        .chain(&chunk::VISUAL[1..])
        .copied()
        .collect();
    every.sort_unstable();
    let mut all = chunk::ALL.to_vec();
    all.sort_unstable();
    assert_eq!(every, all, "every chunk has exactly one domain (SECT in both)");
    Ok(())
}

#[test]
fn the_ground_mesh_covers_the_grid_and_faces_up() -> TestResult {
    let grid = GroundGrid {
        origin_x: 32.0,
        origin_z: -32.0,
        cell_size: 2.0,
        width: 17,
        depth: 17,
        heights: (0..17u16 * 17).map(|i| f32::from(i % 17) * 0.25).collect(),
    };
    let mesh = ground::mesh(&grid);
    mesh.validate()?;
    assert_eq!(mesh.vertices.len(), 17 * 17);
    assert_eq!(mesh.indices.len(), 16 * 16 * 6);
    let position = |k: u32| {
        mesh.vertices
            .get(k as usize)
            .map(|v| v.position)
            .unwrap_or_default()
    };
    for tri in mesh.indices.as_chunks::<3>().0 {
        let [first, second, third] = tri.map(position);
        let edge1 = [second[0] - first[0], second[2] - first[2]];
        let edge2 = [third[0] - first[0], third[2] - first[2]];
        assert!(
            edge1[1] * edge2[0] - edge1[0] * edge2[1] > 0.0,
            "{tri:?} faces up"
        );
    }
    Ok(())
}
