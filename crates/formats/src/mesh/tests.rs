//! Mesh format tests: round trip, every rule, and corruption safety.

use super::*;

type TestResult = Result<(), Box<dyn std::error::Error>>;

/// A unit quad (two triangles) facing +Y, one meshlet.
fn quad(skinned: bool) -> MeshAsset {
    let v = |x: f32, z: f32| MeshVertex {
        position: [x, 0.0, z],
        normal: [0.0, 1.0, 0.0],
        uv0: [x, z],
        uv1: [0.0; 2],
    };
    MeshAsset {
        vertices: vec![v(0.0, 0.0), v(0.0, 1.0), v(1.0, 1.0), v(1.0, 0.0)],
        skin: skinned.then(|| {
            vec![
                SkinInfluence {
                    joints: [0, 1, 0, 0],
                    weights: [200, 55, 0, 0]
                };
                4
            ]
        }),
        indices: vec![0, 1, 2, 0, 2, 3],
        meshlets: vec![Meshlet {
            first_index: 0,
            index_count: 6,
            center: [0.5, 0.0, 0.5],
            radius: 0.71,
            cone_axis: [0.0, -1.0, 0.0],
            cone_cutoff: 0.0,
        }],
        bounds_min: [0.0; 3],
        bounds_max: [1.0, 0.0, 1.0],
    }
}

#[test]
fn round_trips_with_and_without_skinning() -> TestResult {
    for skinned in [false, true] {
        let m = quad(skinned);
        let bytes = m.encode();
        assert_eq!(
            bytes.len(),
            48 + 4 * 40 + if skinned { 4 * 12 } else { 0 } + 6 * 4 + 40
        );
        assert_eq!(MeshAsset::parse(&bytes)?, m);
    }
    Ok(())
}

#[test]
fn rules_reject() {
    let bad = |edit: fn(&mut MeshAsset)| {
        let mut m = quad(true);
        edit(&mut m);
        MeshAsset::parse(&m.encode()).err()
    };
    assert_eq!(
        bad(|m| m.indices.push(0)),
        Some(FormatError::Dimensions),
        "not a triangle list"
    );
    assert_eq!(
        bad(|m| if let Some(i) = m.indices.first_mut() {
            *i = 9;
        }),
        Some(FormatError::Geometry)
    );
    assert_eq!(
        bad(|m| if let Some(v) = m.vertices.first_mut() {
            v.normal = [0.0, 2.0, 0.0];
        }),
        Some(FormatError::Geometry),
        "normals are unit"
    );
    assert_eq!(
        bad(|m| if let Some(v) = m.vertices.first_mut() {
            v.position = [5.0, 0.0, 0.0];
        }),
        Some(FormatError::Geometry),
        "outside the bounds"
    );
    assert_eq!(
        bad(|m| if let Some(mm) = m.meshlets.first_mut() {
            mm.radius = 0.1;
        }),
        Some(FormatError::Geometry),
        "the sphere must contain its triangles"
    );
    assert_eq!(
        bad(|m| if let Some(mm) = m.meshlets.first_mut() {
            mm.index_count = 3;
        }),
        Some(FormatError::Geometry),
        "meshlets cover every index"
    );
    assert_eq!(
        bad(|m| if let Some(s) = m.skin.as_mut().and_then(|s| s.first_mut()) {
            s.weights = [0; 4];
        }),
        Some(FormatError::Inconsistent)
    );
    assert_eq!(
        bad(|m| if let Some(v) = m.vertices.first_mut() {
            v.uv0 = [f32::NAN, 0.0];
        }),
        Some(FormatError::NonFinite)
    );
    let bytes = quad(false).encode();
    let corrupt = |at: usize, v: &[u8]| {
        let mut b = bytes.clone();
        if let Some(s) = b.get_mut(at..at + v.len()) {
            s.copy_from_slice(v);
        }
        MeshAsset::parse(&b).err()
    };
    assert_eq!(corrupt(0, b"MMSX"), Some(FormatError::Magic));
    assert_eq!(corrupt(6, &[2, 0]), Some(FormatError::Flags(2)));
    assert_eq!(corrupt(44, &[1]), Some(FormatError::Reserved));
}

#[test]
fn no_corruption_or_truncation_panics() {
    let bytes = quad(true).encode();
    for cut in 0..bytes.len() {
        assert!(MeshAsset::parse(bytes.get(..cut).unwrap_or(&[])).is_err());
    }
    for i in 0..bytes.len() {
        for mask in [0x01u8, 0x80, 0xff] {
            let mut b = bytes.clone();
            if let Some(x) = b.get_mut(i) {
                *x ^= mask;
            }
            if let Ok(m) = MeshAsset::parse(&b) {
                assert_eq!(MeshAsset::parse(&m.encode()).ok(), Some(m));
            }
        }
    }
}
