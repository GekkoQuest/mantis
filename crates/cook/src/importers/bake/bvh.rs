//! A bounding volume hierarchy over world-space triangles for the bakers' ray casts.
//!
//! The build is deterministic: each node splits its triangles at the median centroid
//! along the longest axis of the centroid bounds, ordering ties by triangle index, until
//! at most [`LEAF_SIZE`] remain (or every centroid coincides). Rays intersect triangles
//! with Möller–Trumbore; [`Bvh::closest`] finds the nearest hit and reports whether it
//! struck the front face (the side `(b - a) x (c - a)` points to, decision 0019),
//! [`Bvh::occluded`] stops at any hit (shadow rays).

use super::math::V3;

/// Most triangles in a leaf.
pub const LEAF_SIZE: usize = 4;
/// Traversal stack depth: median splits keep the tree depth below 34 for any `u32` count.
const STACK: usize = 64;
/// Box-test slack so grazing hits are never culled by rounding (Ize, "Robust BVH ray
/// traversal").
const SLACK: f32 = 1.000_000_4;

/// One triangle, wound so `(b - a) x (c - a)` points out of the front face.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Triangle {
    /// First corner.
    pub a: V3,
    /// Second corner.
    pub b: V3,
    /// Third corner.
    pub c: V3,
}

impl Triangle {
    /// The unit front-face normal (zero for a degenerate triangle).
    pub fn normal(&self) -> V3 {
        (self.b - self.a).cross(self.c - self.a).normalized()
    }

    /// Twice the area.
    pub fn double_area(&self) -> f32 {
        (self.b - self.a).cross(self.c - self.a).length()
    }

    /// Möller–Trumbore: the distance along unit or non-unit `dir` from `origin` to the
    /// hit in `(t_min, t_max)`, and whether the ray struck the front face.
    pub fn intersect(&self, origin: V3, dir: V3, t_min: f32, t_max: f32) -> Option<(f32, bool)> {
        let e1 = self.b - self.a;
        let e2 = self.c - self.a;
        let pvec = dir.cross(e2);
        // det = e1 . (d x e2) = -d . (e1 x e2): positive when the ray meets the front face.
        let det = e1.dot(pvec);
        if det.abs() < 1e-14 {
            return None;
        }
        let inv = 1.0 / det;
        let tvec = origin - self.a;
        let u = tvec.dot(pvec) * inv;
        if !(0.0..=1.0).contains(&u) {
            return None;
        }
        let qvec = tvec.cross(e1);
        let v = dir.dot(qvec) * inv;
        if v < 0.0 || u + v > 1.0 {
            return None;
        }
        let t = e2.dot(qvec) * inv;
        (t > t_min && t < t_max).then_some((t, det > 0.0))
    }

    fn bounds(&self) -> (V3, V3) {
        (self.a.min(self.b).min(self.c), self.a.max(self.b).max(self.c))
    }

    fn centroid(&self) -> V3 {
        (self.a + self.b + self.c) * (1.0 / 3.0)
    }
}

/// A ray hit.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Hit {
    /// Distance along the ray direction (in units of its length).
    pub t: f32,
    /// The triangle's index in the input of [`Bvh::build`].
    pub triangle: u32,
    /// Whether the front face was struck.
    pub front: bool,
    /// The triangle's unit front-face normal.
    pub normal: V3,
}

#[derive(Clone, Copy, Debug)]
struct Node {
    min: V3,
    max: V3,
    /// Leaf: first triangle (in `order`). Interior: the right child (the left child is
    /// the next node).
    first_or_right: u32,
    /// Triangles in a leaf, 0 for an interior node.
    count: u32,
    /// The split axis of an interior node.
    axis: u8,
}

/// A triangle BVH.
#[derive(Clone, Debug, Default)]
pub struct Bvh {
    triangles: Vec<Triangle>,
    /// Input indices of `triangles` (leaf order).
    ids: Vec<u32>,
    nodes: Vec<Node>,
}

struct Builder<'a> {
    input: &'a [Triangle],
    centroids: Vec<V3>,
    nodes: Vec<Node>,
}

impl Builder<'_> {
    fn build(&mut self, order: &mut [u32], first: u32) -> usize {
        let index = self.nodes.len();
        let (mut min, mut max) = (
            V3::new(f32::MAX, f32::MAX, f32::MAX),
            V3::new(f32::MIN, f32::MIN, f32::MIN),
        );
        let (mut cmin, mut cmax) = (min, max);
        for i in order.iter() {
            if let (Some(t), Some(c)) = (self.input.get(*i as usize), self.centroids.get(*i as usize)) {
                let (lo, hi) = t.bounds();
                (min, max) = (min.min(lo), max.max(hi));
                (cmin, cmax) = (cmin.min(*c), cmax.max(*c));
            }
        }
        let count = u32::try_from(order.len()).unwrap_or(u32::MAX);
        self.nodes.push(Node {
            min,
            max,
            first_or_right: first,
            count,
            axis: 0,
        });
        let extent = cmax - cmin;
        let axis = if extent.x >= extent.y && extent.x >= extent.z {
            0
        } else if extent.y >= extent.z {
            1
        } else {
            2
        };
        if order.len() <= LEAF_SIZE || extent.axis(axis) <= 0.0 {
            return index;
        }
        let centroids = &self.centroids;
        let key = |i: u32| centroids.get(i as usize).map_or(0.0, |c| c.axis(axis));
        order.sort_unstable_by(|a, b| key(*a).total_cmp(&key(*b)).then(a.cmp(b)));
        let mid = order.len() / 2;
        let (left, right) = order.split_at_mut(mid);
        self.build(left, first);
        let right_index = self.build(right, first + u32::try_from(mid).unwrap_or(0));
        if let Some(node) = self.nodes.get_mut(index) {
            node.first_or_right = u32::try_from(right_index).unwrap_or(0);
            node.count = 0;
            node.axis = u8::try_from(axis).unwrap_or(0);
        }
        index
    }
}

fn slab(node: &Node, origin: V3, inv: V3, t_max: f32) -> bool {
    let mut near = 0.0f32;
    let mut far = t_max;
    for axis in 0..3 {
        let (o, i) = (origin.axis(axis), inv.axis(axis));
        let t0 = (node.min.axis(axis) - o) * i;
        let t1 = (node.max.axis(axis) - o) * i;
        // `min`/`max` drop the NaN of `0 * inf` (an origin on a slab plane).
        near = near.max(t0.min(t1));
        far = far.min(t0.max(t1));
    }
    near <= far * SLACK
}

impl Bvh {
    /// Builds the hierarchy over `triangles` (deterministic for the same input).
    pub fn build(triangles: &[Triangle]) -> Bvh {
        if triangles.is_empty() {
            return Bvh::default();
        }
        let mut order: Vec<u32> = (0..u32::try_from(triangles.len()).unwrap_or(u32::MAX)).collect();
        let mut builder = Builder {
            input: triangles,
            centroids: triangles.iter().map(Triangle::centroid).collect(),
            nodes: Vec::new(),
        };
        builder.build(&mut order, 0);
        Bvh {
            triangles: order
                .iter()
                .filter_map(|i| triangles.get(*i as usize).copied())
                .collect(),
            ids: order,
            nodes: builder.nodes,
        }
    }

    /// Triangles in the hierarchy.
    pub fn len(&self) -> usize {
        self.triangles.len()
    }

    /// Whether it holds no triangle.
    pub fn is_empty(&self) -> bool {
        self.triangles.is_empty()
    }

    /// Visits leaves the ray may reach before `t_max` (which `visit` may shrink) in
    /// near-to-far order; `visit` returns `true` to stop.
    fn walk(
        &self,
        origin: V3,
        dir: V3,
        mut visit: impl FnMut(&[Triangle], u32, &mut f32) -> bool,
        mut t_max: f32,
    ) {
        if self.nodes.is_empty() {
            return;
        }
        let inv = V3::new(1.0 / dir.x, 1.0 / dir.y, 1.0 / dir.z);
        let mut stack = [0u32; STACK];
        let mut top = 1usize;
        while top > 0 {
            top -= 1;
            let Some(node) = stack.get(top).and_then(|n| self.nodes.get(*n as usize)) else {
                continue;
            };
            if !slab(node, origin, inv, t_max) {
                continue;
            }
            if node.count > 0 {
                let first = node.first_or_right as usize;
                let tris = self
                    .triangles
                    .get(first..first + node.count as usize)
                    .unwrap_or(&[]);
                if visit(tris, node.first_or_right, &mut t_max) {
                    return;
                }
                continue;
            }
            let here = stack.get(top).copied().unwrap_or(0);
            let (left, right) = (here + 1, node.first_or_right);
            // Push the far child first so the near one is visited first.
            let (far, near) = if dir.axis(usize::from(node.axis)) >= 0.0 {
                (right, left)
            } else {
                (left, right)
            };
            for child in [far, near] {
                if let Some(slot) = stack.get_mut(top) {
                    *slot = child;
                    top += 1;
                }
            }
        }
    }

    /// The nearest hit in `(t_min, t_max)`.
    pub fn closest(&self, origin: V3, dir: V3, t_min: f32, t_max: f32) -> Option<Hit> {
        let mut best: Option<(f32, u32, bool)> = None;
        self.walk(
            origin,
            dir,
            |tris, first, limit| {
                for (tri, pos) in tris.iter().zip(first..) {
                    if let Some((t, front)) = tri.intersect(origin, dir, t_min, *limit) {
                        *limit = t;
                        best = Some((t, pos, front));
                    }
                }
                false
            },
            t_max,
        );
        best.map(|(t, pos, front)| Hit {
            t,
            triangle: self.ids.get(pos as usize).copied().unwrap_or(0),
            front,
            normal: self
                .triangles
                .get(pos as usize)
                .map_or(V3::ZERO, Triangle::normal),
        })
    }

    /// Whether anything lies along the ray in `(t_min, t_max)`.
    pub fn occluded(&self, origin: V3, dir: V3, t_min: f32, t_max: f32) -> bool {
        let mut hit = false;
        self.walk(
            origin,
            dir,
            |tris, _, limit| {
                hit = tris
                    .iter()
                    .any(|tri| tri.intersect(origin, dir, t_min, *limit).is_some());
                hit
            },
            t_max,
        );
        hit
    }
}
