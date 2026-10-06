//! VAT baking tests.

use super::*;
use crate::clip::tests::{clip_asset, track};
use crate::skeleton::tests::{TestResult, leg};
use core::f32::consts::FRAC_PI_2;
use glam::Quat;
use mantis_formats::anim_clip::Channel;

const POSITIONS: [[f32; 3]; 3] = [[0.0, 0.0, 0.5], [0.0, 1.0, 0.0], [0.2, 1.5, 0.0]];
const NORMALS: [[f32; 3]; 3] = [[0.0, 0.0, 1.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]];
const JOINTS: [[u16; 4]; 3] = [[0, 0, 0, 0], [1, 0, 0, 0], [1, 2, 0, 0]];
const WEIGHTS: [[f32; 4]; 3] = [[1.0, 0.0, 0.0, 0.0], [2.0, 0.0, 0.0, 0.0], [0.5, 0.5, 0.0, 0.0]];

fn mesh() -> SkinnedMesh<'static> {
    SkinnedMesh {
        positions: &POSITIONS,
        normals: &NORMALS,
        joints: &JOINTS,
        weights: &WEIGHTS,
    }
}

fn close(a: [f32; 4], b: [f32; 4]) -> bool {
    a.iter().zip(&b).all(|(x, y)| (x - y).abs() < 1e-5)
}

#[test]
fn bind_pose_reproduces_the_mesh() -> TestResult {
    let s = leg()?;
    let still = Clip::new(&clip_asset(1.0, true, vec![]))?;
    let vat = bake_vat(&s, &still, &mesh(), 10.0)?;
    assert_eq!((vat.frame_count, vat.vertex_count), (10, 3));
    assert!(vat.looping);
    assert!((vat.seconds_per_frame - 0.1).abs() < 1e-6);
    assert_eq!(vat.positions.len(), 30);
    for frame in 0..vat.frame_count {
        for (v, ([px, py, pz], [nx, ny, nz])) in (0u32..).zip(POSITIONS.iter().zip(&NORMALS)) {
            assert!(close(
                vat.position(frame, v).ok_or("texel")?,
                [*px, *py, *pz, 1.0]
            ));
            assert!(close(vat.normal(frame, v).ok_or("texel")?, [*nx, *ny, *nz, 0.0]));
        }
    }
    assert_eq!(vat.bounds_min, [0.0, 0.0, 0.0]);
    assert_eq!(vat.bounds_max, [0.2, 1.5, 0.5]);
    Ok(())
}

#[test]
fn rotated_bone_moves_its_vertices() -> TestResult {
    let s = leg()?;
    // The hip (at y = 2) turns 90 degrees about z for the whole clip.
    let q = Quat::from_rotation_z(FRAC_PI_2);
    let turned = Clip::new(&clip_asset(
        1.0,
        false,
        vec![track(1, Channel::Rotation, &[0.0], &[q.x, q.y, q.z, q.w])],
    ))?;
    let vat = bake_vat(&s, &turned, &mesh(), 4.0)?;
    assert_eq!(vat.frame_count, 5, "clamped clips include both ends");
    for frame in 0..vat.frame_count {
        // Root-weighted vertex: unchanged.
        assert!(close(
            vat.position(frame, 0).ok_or("texel")?,
            [0.0, 0.0, 0.5, 1.0]
        ));
        // Hip-weighted vertex 1 below the hip swings to +x: (0, -1, 0) -> (1, 0, 0).
        assert!(close(
            vat.position(frame, 1).ok_or("texel")?,
            [1.0, 2.0, 0.0, 1.0]
        ));
        assert!(close(vat.normal(frame, 1).ok_or("texel")?, [0.0, 1.0, 0.0, 0.0]));
        // Split between hip and knee, which turn together: (0.2, -0.5, 0) -> (0.5, 0.2, 0).
        assert!(close(
            vat.position(frame, 2).ok_or("texel")?,
            [0.5, 2.2, 0.0, 1.0]
        ));
    }
    Ok(())
}

#[test]
fn rows_are_frames() -> TestResult {
    let s = leg()?;
    // The root slides 1 unit along x over the clip.
    let slide = Clip::new(&clip_asset(
        1.0,
        false,
        vec![track(
            0,
            Channel::Translation,
            &[0.0, 1.0],
            &[0.0, 0.0, 0.0, 1.0, 0.0, 0.0],
        )],
    ))?;
    let vat = bake_vat(&s, &slide, &mesh(), 2.0)?;
    assert_eq!(vat.frame_count, 3);
    for (frame, x) in [(0u32, 0.0f32), (1, 0.5), (2, 1.0)] {
        let texel = vat.texel(frame, 0).ok_or("texel")?;
        assert_eq!(texel, frame as usize * 3);
        let [px, ..] = *vat.positions.get(texel).ok_or("position")?;
        assert!((px - x).abs() < 1e-5, "frame {frame}: {px}");
    }
    let [max_x, ..] = vat.bounds_max;
    assert!((max_x - 1.2).abs() < 1e-5);
    assert_eq!(vat.texel(3, 0), None);
    assert_eq!(vat.texel(0, 3), None);
    Ok(())
}

#[test]
fn rejects_bad_input() -> TestResult {
    let s = leg()?;
    let still = Clip::new(&clip_asset(1.0, true, vec![]))?;
    assert_eq!(
        bake_vat(&s, &still, &mesh(), 0.0).err(),
        Some(AnimError::InvalidFrameRate)
    );
    assert_eq!(
        bake_vat(&s, &still, &mesh(), f32::NAN).err(),
        Some(AnimError::InvalidFrameRate)
    );
    let short = SkinnedMesh {
        normals: NORMALS.get(..2).ok_or("slice")?,
        ..mesh()
    };
    assert!(matches!(
        bake_vat(&s, &still, &short, 10.0),
        Err(AnimError::InvalidMesh(_))
    ));
    let zero = [[0.0f32; 4]; 3];
    let unweighted = SkinnedMesh {
        weights: &zero,
        ..mesh()
    };
    assert_eq!(
        bake_vat(&s, &still, &unweighted, 10.0).err(),
        Some(AnimError::InvalidMesh(0))
    );
    let joints = [[0u16; 4], [9, 0, 0, 0], [0; 4]];
    let bad_joint = SkinnedMesh {
        joints: &joints,
        ..mesh()
    };
    assert_eq!(
        bake_vat(&s, &still, &bad_joint, 10.0).err(),
        Some(AnimError::InvalidMesh(1))
    );
    let mut other = clip_asset(1.0, true, vec![]);
    other.bone_count = 5;
    let other = Clip::new(&other)?;
    assert!(matches!(
        bake_vat(&s, &other, &mesh(), 10.0),
        Err(AnimError::BoneCountMismatch {
            expected: 4,
            actual: 5
        })
    ));
    assert_eq!(
        bake_vat(&s, &still, &mesh(), 1e9).err(),
        Some(AnimError::VatTooLarge)
    );
    Ok(())
}
