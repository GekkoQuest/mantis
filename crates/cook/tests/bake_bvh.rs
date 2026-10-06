//! The bakers' building blocks: the BVH against brute force, outward box winding, and
//! the atlas packer.

#![allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]

use mantis_cook::importers::bake::bvh::{Bvh, Triangle};
use mantis_cook::importers::bake::lightmap::pack;
use mantis_cook::importers::bake::math::V3;
use mantis_cook::importers::bake::scene::box_triangles;

type TestResult = Result<(), Box<dyn std::error::Error>>;

/// A fixed-seed generator (test data only; the bakers themselves use no randomness).
struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> f32 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        ((self.0 >> 40) as f32) / ((1u64 << 24) as f32)
    }

    fn range(&mut self, lo: f32, hi: f32) -> f32 {
        lo + (hi - lo) * self.next()
    }

    fn point(&mut self, lo: f32, hi: f32) -> V3 {
        V3::new(self.range(lo, hi), self.range(lo, hi), self.range(lo, hi))
    }
}

fn scene(rng: &mut Lcg) -> Vec<Triangle> {
    let mut tris = Vec::new();
    for _ in 0..300 {
        let a = rng.point(-10.0, 10.0);
        tris.push(Triangle {
            a,
            b: a + rng.point(-2.0, 2.0),
            c: a + rng.point(-2.0, 2.0),
        });
    }
    for i in 0..8 {
        let lo = rng.point(-8.0, 6.0);
        let size = V3::new(1.0 + i as f32 * 0.2, 0.5, 2.0);
        tris.extend(box_triangles(lo.to_array(), (lo + size).to_array()));
    }
    tris
}

fn brute(tris: &[Triangle], origin: V3, dir: V3) -> Option<(f32, bool)> {
    tris.iter()
        .filter_map(|t| t.intersect(origin, dir, 1e-4, f32::INFINITY))
        .min_by(|a, b| a.0.total_cmp(&b.0))
}

#[test]
fn bvh_hits_match_brute_force() -> TestResult {
    let mut rng = Lcg(7);
    let tris = scene(&mut rng);
    let bvh = Bvh::build(&tris);
    assert_eq!(bvh.len(), tris.len());
    let (mut hits, mut misses) = (0, 0);
    for _ in 0..4000 {
        let origin = rng.point(-14.0, 14.0);
        let dir = rng.point(-1.0, 1.0).normalized();
        if dir == V3::ZERO {
            continue;
        }
        let expected = brute(&tris, origin, dir);
        let got = bvh.closest(origin, dir, 1e-4, f32::INFINITY);
        assert_eq!(got.map(|h| (h.t, h.front)), expected, "ray {origin:?} {dir:?}");
        if let Some(h) = got {
            let t = tris.get(h.triangle as usize).ok_or("triangle id")?;
            assert_eq!(t.intersect(origin, dir, 1e-4, f32::INFINITY), expected);
            assert!((h.normal.dot(t.normal()) - 1.0).abs() < 1e-5);
            hits += 1;
        } else {
            misses += 1;
        }
        assert_eq!(bvh.occluded(origin, dir, 1e-4, f32::INFINITY), expected.is_some());
        if let Some((t, _)) = expected {
            assert!(
                !bvh.occluded(origin, dir, 1e-4, t * 0.999),
                "nothing before the nearest hit"
            );
        }
    }
    assert!(hits > 500 && misses > 500, "{hits} hits, {misses} misses");
    Ok(())
}

#[test]
fn bvh_build_is_deterministic_and_handles_degenerate_input() -> TestResult {
    let mut rng = Lcg(11);
    let tris = scene(&mut rng);
    let a = Bvh::build(&tris);
    let b = Bvh::build(&tris);
    let mut rng = Lcg(3);
    for _ in 0..200 {
        let (o, d) = (rng.point(-12.0, 12.0), rng.point(-1.0, 1.0).normalized());
        assert_eq!(
            a.closest(o, d, 1e-4, f32::INFINITY),
            b.closest(o, d, 1e-4, f32::INFINITY)
        );
    }
    let empty = Bvh::build(&[]);
    assert!(empty.is_empty());
    assert!(
        empty
            .closest(V3::ZERO, V3::new(0.0, 1.0, 0.0), 0.0, f32::INFINITY)
            .is_none()
    );
    // Every centroid in one place: one leaf, still correct.
    let stack: Vec<Triangle> = (0..20)
        .map(|_| Triangle {
            a: V3::new(-1.0, 0.0, -1.0),
            b: V3::new(-1.0, 0.0, 1.0),
            c: V3::new(1.0, 0.0, 0.0),
        })
        .collect();
    let hit = Bvh::build(&stack)
        .closest(
            V3::new(-0.5, 1.0, 0.0),
            V3::new(0.0, -1.0, 0.0),
            0.0,
            f32::INFINITY,
        )
        .ok_or("the stacked triangles are hit")?;
    assert_eq!((hit.t, hit.front), (1.0, true));
    Ok(())
}

#[test]
fn box_triangles_face_outward() -> TestResult {
    let tris = box_triangles([0.0, 0.0, 0.0], [2.0, 1.0, 3.0]);
    assert_eq!(tris.len(), 12);
    let center = V3::new(1.0, 0.5, 1.5);
    for t in &tris {
        let centroid = (t.a + t.b + t.c) * (1.0 / 3.0);
        assert!(t.normal().dot(centroid - center) > 0.0, "{t:?}");
    }
    // A ray from outside meets a front face; from inside, a back face.
    let bvh = Bvh::build(&tris);
    let down = V3::new(0.0, -1.0, 0.0);
    let outside = bvh
        .closest(V3::new(1.0, 5.0, 1.0), down, 0.0, f32::INFINITY)
        .ok_or("hit from outside")?;
    let inside = bvh
        .closest(V3::new(1.0, 0.5, 1.0), down, 0.0, f32::INFINITY)
        .ok_or("hit from inside")?;
    assert!(outside.front && !inside.front);
    Ok(())
}

#[test]
fn packed_rectangles_never_overlap_and_stay_inside() -> TestResult {
    let mut rng = Lcg(5);
    let sizes: Vec<(u32, u32)> = (0..150)
        .map(|_| {
            let s = 4 + (rng.next() * 40.0) as u32;
            (s, s)
        })
        .collect();
    let (w, h, rects) = pack(&sizes, 1024).ok_or("fits")?;
    assert!(w.is_power_of_two() && h <= w, "{w}x{h}");
    assert_eq!(rects.len(), sizes.len());
    for (i, (r, s)) in rects.iter().zip(&sizes).enumerate() {
        assert_eq!((r.w, r.h), *s, "sizes are kept, in input order");
        assert!(r.x + r.w <= w && r.y + r.h <= h);
        for q in rects.iter().skip(i + 1) {
            let apart = r.x + r.w <= q.x || q.x + q.w <= r.x || r.y + r.h <= q.y || q.y + q.h <= r.y;
            assert!(apart, "{r:?} overlaps {q:?}");
        }
    }
    assert_eq!(pack(&sizes, 64), None, "too much for a 64 x 64 atlas");
    assert_eq!(pack(&[(80, 80)], 64), None, "wider than the atlas");
    assert_eq!(pack(&sizes, 1024), Some((w, h, rects)), "deterministic");
    Ok(())
}
