//! Meshlet building: greedy, spatially coherent clusters of at most
//! [`MESHLET_MAX_TRIANGLES`] triangles and [`MESHLET_MAX_VERTICES`] distinct vertices,
//! with a bounding sphere (Ritter's center, exact radius) and a conservative normal cone.
//!
//! A meshlet grows from a seed triangle by adjacency (triangles sharing a position),
//! always taking the candidate that adds the fewest new vertices, then the one nearest
//! the meshlet's centroid, then the lowest index. Seeds, and the fallback when a meshlet
//! has no neighbor left, follow a Morton order of triangle centroids, so disconnected
//! pieces still cluster by locality.

use mantis_formats::mesh::{MESHLET_MAX_TRIANGLES, MESHLET_MAX_VERTICES};

/// Cone cutoff margin: the stored cutoff is raised by this much so float error in the
/// renderer's test never culls a visible triangle.
const CONE_MARGIN: f64 = 1e-3;

/// `a - b`.
pub(crate) fn sub(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}

/// `a x b`.
pub(crate) fn cross(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}

/// `a . b`.
pub(crate) fn dot(a: [f32; 3], b: [f32; 3]) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

/// `v` normalized, or `None` when it has no direction.
pub(crate) fn normalize(v: [f32; 3]) -> Option<[f32; 3]> {
    let len = dot(v, v).sqrt();
    (len > 1e-20 && len.is_finite()).then(|| v.map(|c| c / len))
}

/// The face normal `(b - a) x (c - a)` (length twice the area).
pub(crate) fn face_normal(p: [[f32; 3]; 3]) -> [f32; 3] {
    let [a, b, c] = p;
    cross(sub(b, a), sub(c, a))
}

fn morton_key(c: [f32; 3], lo: [f32; 3], hi: [f32; 3]) -> u32 {
    let mut key = 0u32;
    let q = c.iter().zip(lo).zip(hi).map(|((v, l), h)| {
        let t = if h > l { (v - l) / (h - l) } else { 0.0 };
        #[expect(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // Clamped to 0..=1023.
        let q = (t.clamp(0.0, 1.0) * 1023.0) as u32;
        q
    });
    for (axis, v) in (0u32..).zip(q) {
        for bit in 0..10 {
            key |= ((v >> bit) & 1) << (bit * 3 + axis);
        }
    }
    key
}

/// Groups triangles into meshlets. `tris` are vertex indices, `adjacency` the position
/// ids of each triangle's corners (vertices that differ only in normal or UV share one),
/// `centroids` each triangle's centroid. Returns triangle indices per meshlet; every
/// triangle appears exactly once.
pub(crate) fn cluster(
    tris: &[[u32; 3]],
    adjacency: &[[u32; 3]],
    centroids: &[[f32; 3]],
    position_count: usize,
) -> Vec<Vec<usize>> {
    let n = tris.len();
    let mut lo = [f32::INFINITY; 3];
    let mut hi = [f32::NEG_INFINITY; 3];
    for c in centroids {
        for ((l, h), v) in lo.iter_mut().zip(hi.iter_mut()).zip(c) {
            *l = l.min(*v);
            *h = h.max(*v);
        }
    }
    let mut order: Vec<(u32, usize)> = centroids
        .iter()
        .enumerate()
        .map(|(i, c)| (morton_key(*c, lo, hi), i))
        .collect();
    order.sort_unstable();
    let mut by_position: Vec<Vec<usize>> = vec![Vec::new(); position_count];
    for (t, ids) in adjacency.iter().enumerate() {
        for id in ids {
            if let Some(list) = by_position.get_mut(*id as usize) {
                list.push(t);
            }
        }
    }
    let mut used = vec![false; n];
    let mut queued = vec![usize::MAX; n];
    let mut cursor = 0usize;
    let mut clusters = Vec::new();
    loop {
        while order
            .get(cursor)
            .is_some_and(|(_, t)| used.get(*t).copied().unwrap_or(true))
        {
            cursor += 1;
        }
        let Some(&(_, seed)) = order.get(cursor) else {
            break;
        };
        let id = clusters.len();
        let mut grow = Grow {
            tris,
            centroids,
            triangles: Vec::new(),
            vertices: Vec::new(),
            candidates: Vec::new(),
            sum: [0.0; 3],
        };
        let mut next = Some(seed);
        while let Some(t) = next {
            grow.add(t, &mut used);
            for pid in adjacency.get(t).copied().unwrap_or_default() {
                for &c in by_position.get(pid as usize).map_or(&[][..], Vec::as_slice) {
                    if let Some(q) = queued.get_mut(c)
                        && *q != id
                        && !used.get(c).copied().unwrap_or(true)
                    {
                        *q = id;
                        grow.candidates.push(c);
                    }
                }
            }
            if grow.triangles.len() >= MESHLET_MAX_TRIANGLES as usize {
                break;
            }
            next = grow.pick(&used).or_else(|| {
                while order
                    .get(cursor)
                    .is_some_and(|(_, t)| used.get(*t).copied().unwrap_or(true))
                {
                    cursor += 1;
                }
                order.get(cursor).map(|(_, t)| *t)
            });
            if let Some(t) = next
                && grow.vertices.len() + grow.new_vertices(t) > MESHLET_MAX_VERTICES
            {
                next = None;
            }
        }
        clusters.push(grow.triangles);
    }
    clusters
}

struct Grow<'a> {
    tris: &'a [[u32; 3]],
    centroids: &'a [[f32; 3]],
    triangles: Vec<usize>,
    vertices: Vec<u32>,
    candidates: Vec<usize>,
    sum: [f32; 3],
}

impl Grow<'_> {
    fn new_vertices(&self, t: usize) -> usize {
        let corners = self.tris.get(t).copied().unwrap_or_default();
        corners
            .iter()
            .enumerate()
            .filter(|(i, v)| !self.vertices.contains(v) && !corners.get(..*i).is_some_and(|p| p.contains(v)))
            .count()
    }

    fn add(&mut self, t: usize, used: &mut [bool]) {
        if let Some(u) = used.get_mut(t) {
            *u = true;
        }
        self.triangles.push(t);
        for v in self.tris.get(t).copied().unwrap_or_default() {
            if !self.vertices.contains(&v) {
                self.vertices.push(v);
            }
        }
        let c = self.centroids.get(t).copied().unwrap_or_default();
        self.sum = [self.sum[0] + c[0], self.sum[1] + c[1], self.sum[2] + c[2]];
    }

    fn pick(&mut self, used: &[bool]) -> Option<usize> {
        self.candidates.retain(|c| !used.get(*c).copied().unwrap_or(true));
        #[expect(clippy::cast_precision_loss)] // A meshlet holds at most 124 triangles.
        let k = self.triangles.len().max(1) as f32;
        let center = self.sum.map(|s| s / k);
        self.candidates
            .iter()
            .map(|&c| {
                let d = sub(self.centroids.get(c).copied().unwrap_or_default(), center);
                (self.new_vertices(c), dot(d, d), c)
            })
            .min_by(|a, b| a.0.cmp(&b.0).then(a.1.total_cmp(&b.1)).then(a.2.cmp(&b.2)))
            .map(|(_, _, c)| c)
    }
}

/// A bounding sphere containing every point: Ritter's center, then the exact largest
/// distance (computed in `f64`) rounded up.
pub(crate) fn bounding_sphere(points: &[[f32; 3]]) -> ([f32; 3], f32) {
    let to64 = |p: [f32; 3]| p.map(f64::from);
    let d2 = |a: [f64; 3], b: [f64; 3]| {
        let d = [a[0] - b[0], a[1] - b[1], a[2] - b[2]];
        d[0] * d[0] + d[1] * d[1] + d[2] * d[2]
    };
    let Some(&first) = points.first() else {
        return ([0.0; 3], 0.0);
    };
    let farthest = |from: [f64; 3]| {
        points
            .iter()
            .map(|p| to64(*p))
            .max_by(|a, b| d2(*a, from).total_cmp(&d2(*b, from)))
            .unwrap_or(from)
    };
    let a = farthest(to64(first));
    let b = farthest(a);
    let mut center = [
        f64::midpoint(a[0], b[0]),
        f64::midpoint(a[1], b[1]),
        f64::midpoint(a[2], b[2]),
    ];
    let mut radius = d2(a, b).sqrt() / 2.0;
    for p in points.iter().map(|p| to64(*p)) {
        let d = d2(p, center).sqrt();
        if d > radius {
            let grow = (d - radius) / 2.0;
            radius += grow;
            let t = grow / d;
            center = [
                center[0] + (p[0] - center[0]) * t,
                center[1] + (p[1] - center[1]) * t,
                center[2] + (p[2] - center[2]) * t,
            ];
        }
    }
    #[expect(clippy::cast_possible_truncation)] // Model-space coordinates, within f32.
    let center32 = center.map(|c| c as f32);
    let c64 = to64(center32);
    let exact = points
        .iter()
        .map(|p| d2(to64(*p), c64))
        .fold(0.0f64, f64::max)
        .sqrt();
    #[expect(clippy::cast_possible_truncation)] // Rounded up below.
    let r = (exact * (1.0 + 1e-6)) as f32;
    (center32, r.next_up())
}

/// The normal cone of unit triangle normals: the axis is their normalized average and
/// the cutoff `sin(a) + margin`, where `a` is the largest angle between the axis and a
/// normal. A view direction `v` (toward the meshlet) with `dot(v, axis) >= cutoff` is
/// within `90 - a` degrees of the axis, so it is within 90 degrees of every normal and
/// sees only back faces. An open cone (zero axis, cutoff 1) when the spread reaches 90
/// degrees or there is no normal.
pub(crate) fn normal_cone(normals: &[[f32; 3]]) -> ([f32; 3], f32) {
    const OPEN: ([f32; 3], f32) = ([0.0; 3], 1.0);
    let mut sum = [0.0f64; 3];
    for n in normals {
        for (s, c) in sum.iter_mut().zip(n) {
            *s += f64::from(*c);
        }
    }
    let len = (sum[0] * sum[0] + sum[1] * sum[1] + sum[2] * sum[2]).sqrt();
    if normals.is_empty() || len < 1e-6 {
        return OPEN;
    }
    #[expect(clippy::cast_possible_truncation)] // A unit vector.
    let axis = sum.map(|c| (c / len) as f32);
    let Some(axis) = normalize(axis) else {
        return OPEN;
    };
    let min_dot = normals
        .iter()
        .map(|n| {
            f64::from(axis[0]) * f64::from(n[0])
                + f64::from(axis[1]) * f64::from(n[1])
                + f64::from(axis[2]) * f64::from(n[2])
        })
        .fold(1.0f64, f64::min);
    if min_dot <= 0.0 {
        return OPEN;
    }
    let cutoff = (1.0 - min_dot.min(1.0).powi(2)).sqrt() + CONE_MARGIN;
    if cutoff >= 1.0 {
        return OPEN;
    }
    #[expect(clippy::cast_possible_truncation)] // In (0, 1).
    let cutoff = cutoff as f32;
    (axis, cutoff)
}
