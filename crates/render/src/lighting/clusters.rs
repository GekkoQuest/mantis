//! Clustered forward+ light assignment.
//!
//! The view frustum is divided into `tiles_x x tiles_y` screen tiles and `slices`
//! exponentially spaced depth slices. Each frame every point light is tested against the
//! clusters it can touch (its depth-slice range, every tile), and the result is a compact
//! per-cluster `(offset, count)` table plus a flat light-index list, uploaded for the
//! forward pass. Assignment runs on the CPU into fixed-capacity storage and never
//! allocates; overflow is counted, never grown.
//!
//! Tile row 0 is the top of the screen (matching fragment coordinates).

use bytemuck::{Pod, Zeroable};
use glam::{Mat4, Vec3};

use crate::math::Aabb;

/// Cluster grid and capacity configuration.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct ClusterConfig {
    /// Tiles across.
    pub tiles_x: u32,
    /// Tiles down.
    pub tiles_y: u32,
    /// Depth slices.
    pub slices: u32,
    /// Depth of the first slice boundary (view-space distance).
    pub near: f32,
    /// Depth beyond which point lights are not clustered.
    pub far: f32,
    /// Most point lights per frame.
    pub max_lights: u32,
    /// Most light references across all clusters.
    pub max_indices: u32,
}

impl Default for ClusterConfig {
    fn default() -> Self {
        Self {
            tiles_x: 16,
            tiles_y: 9,
            slices: 24,
            near: 0.5,
            far: 400.0,
            max_lights: 1024,
            max_indices: 65_536,
        }
    }
}

impl ClusterConfig {
    /// Total clusters.
    pub fn cluster_count(&self) -> u32 {
        self.tiles_x * self.tiles_y * self.slices
    }
}

/// A point light.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct PointLight {
    /// World position.
    pub position: Vec3,
    /// Distance at which the light reaches zero.
    pub range: f32,
    /// Linear color.
    pub color: Vec3,
    /// Intensity multiplier.
    pub intensity: f32,
}

/// A point light as the GPU reads it.
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Debug, Pod, Zeroable)]
pub struct GpuPointLight {
    /// World position.
    pub position: [f32; 3],
    /// Range.
    pub range: f32,
    /// Color.
    pub color: [f32; 3],
    /// Intensity.
    pub intensity: f32,
}

/// Cluster grid parameters as the GPU reads them.
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Debug, Pod, Zeroable)]
pub struct GpuClusterParams {
    /// Tiles across, down, slices, and light count.
    pub dims: [u32; 4],
    /// near, far, `slices / ln(far / near)`, tile size in pixels (x).
    pub depth: [f32; 4],
    /// Tile size in pixels (y), padding.
    pub tile: [f32; 4],
}

/// Assignment statistics for one frame.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct ClusterStats {
    /// Lights considered.
    pub lights: u32,
    /// Light references written.
    pub references: u32,
    /// Lights dropped because `max_lights` was reached.
    pub lights_dropped: u32,
    /// References dropped because `max_indices` was reached.
    pub references_dropped: u32,
}

/// The cluster grid and its per-frame assignment.
#[derive(Clone, Debug)]
pub struct Clusters {
    config: ClusterConfig,
    aabbs: Vec<Aabb>,
    grid: Vec<[u32; 2]>,
    indices: Vec<u32>,
    lights: Vec<GpuPointLight>,
    pairs: Vec<(u32, u32)>,
    stats: ClusterStats,
}

impl Clusters {
    /// A grid for a perspective projection with vertical field of view `fov_y` (radians)
    /// and `aspect` (width over height).
    pub fn new(config: ClusterConfig, fov_y: f32, aspect: f32) -> Self {
        let count = config.cluster_count() as usize;
        let mut c = Self {
            config,
            aabbs: Vec::with_capacity(count),
            grid: vec![[0, 0]; count],
            indices: Vec::with_capacity(config.max_indices as usize),
            lights: Vec::with_capacity(config.max_lights as usize),
            pairs: Vec::with_capacity(config.max_indices as usize),
            stats: ClusterStats::default(),
        };
        c.set_projection(fov_y, aspect);
        c
    }

    /// The configuration.
    pub fn config(&self) -> &ClusterConfig {
        &self.config
    }

    /// Slice boundary depth `k` (0 = near, `slices` = far).
    #[expect(clippy::cast_precision_loss)] // Slice counts are small.
    pub fn slice_depth(&self, k: u32) -> f32 {
        let c = &self.config;
        c.near * (c.far / c.near).powf(k as f32 / c.slices as f32)
    }

    /// The slice containing view-space depth `d` (positive distance), clamped.
    #[expect(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    pub fn slice_of(&self, d: f32) -> u32 {
        let c = &self.config;
        // Also catches NaN: anything not beyond the near plane is slice 0.
        if d.partial_cmp(&c.near) != Some(core::cmp::Ordering::Greater) {
            return 0;
        }
        let s = ((d / c.near).ln() / (c.far / c.near).ln() * c.slices as f32).floor();
        (s.max(0.0) as u32).min(c.slices - 1)
    }

    /// Rebuilds cluster bounds for a new projection (resize, field-of-view change).
    #[expect(clippy::cast_precision_loss)] // Tile counts are small.
    pub fn set_projection(&mut self, fov_y: f32, aspect: f32) {
        let c = self.config;
        let ky = (fov_y * 0.5).tan();
        let kx = ky * aspect;
        self.aabbs.clear();
        for k in 0..c.slices {
            let (d0, d1) = (self.slice_depth(k), self.slice_depth(k + 1));
            for ty in 0..c.tiles_y {
                // Row 0 is the top of the screen.
                let y_hi = 1.0 - 2.0 * ty as f32 / c.tiles_y as f32;
                let y_lo = 1.0 - 2.0 * (ty + 1) as f32 / c.tiles_y as f32;
                for tx in 0..c.tiles_x {
                    let x_lo = -1.0 + 2.0 * tx as f32 / c.tiles_x as f32;
                    let x_hi = -1.0 + 2.0 * (tx + 1) as f32 / c.tiles_x as f32;
                    let xs = [x_lo * kx * d0, x_hi * kx * d0, x_lo * kx * d1, x_hi * kx * d1];
                    let ys = [y_lo * ky * d0, y_hi * ky * d0, y_lo * ky * d1, y_hi * ky * d1];
                    let fold = |v: [f32; 4], f: fn(f32, f32) -> f32| v.into_iter().fold(v[0], f);
                    self.aabbs.push(Aabb {
                        // Left-handed view space (decision 0019): depth is +z.
                        min: Vec3::new(fold(xs, f32::min), fold(ys, f32::min), d0),
                        max: Vec3::new(fold(xs, f32::max), fold(ys, f32::max), d1),
                    });
                }
            }
        }
    }

    /// Bounds of cluster `i` in view space.
    pub fn aabb(&self, i: usize) -> Option<&Aabb> {
        self.aabbs.get(i)
    }

    fn cluster_index(&self, tx: u32, ty: u32, slice: u32) -> u32 {
        tx + ty * self.config.tiles_x + slice * self.config.tiles_x * self.config.tiles_y
    }

    /// Assigns `lights` to clusters for the camera's `view` matrix. Allocation-free.
    pub fn assign(&mut self, view: &Mat4, lights: &[PointLight]) -> ClusterStats {
        let c = self.config;
        let mut stats = ClusterStats::default();
        self.lights.clear();
        self.pairs.clear();
        for light in lights {
            if !light.position.is_finite()
                || light.range.partial_cmp(&0.0) != Some(core::cmp::Ordering::Greater)
            {
                continue;
            }
            if self.lights.len() >= c.max_lights as usize {
                stats.lights_dropped += 1;
                continue;
            }
            let li = u32::try_from(self.lights.len()).unwrap_or(u32::MAX);
            self.lights.push(GpuPointLight {
                position: light.position.to_array(),
                range: light.range,
                color: light.color.to_array(),
                intensity: light.intensity,
            });
            stats.lights += 1;
            let center = view.transform_point3(light.position);
            // Left-handed view space (decision 0019): depth is +z.
            let depth = center.z;
            if depth + light.range < 0.0 || depth - light.range > c.far {
                continue;
            }
            let (s0, s1) = (
                self.slice_of(depth - light.range),
                self.slice_of(depth + light.range),
            );
            for slice in s0..=s1 {
                for ty in 0..c.tiles_y {
                    for tx in 0..c.tiles_x {
                        let ci = self.cluster_index(tx, ty, slice);
                        let Some(b) = self.aabbs.get(ci as usize) else {
                            continue;
                        };
                        let closest = center.clamp(b.min, b.max);
                        if (closest - center).length_squared() > light.range * light.range {
                            continue;
                        }
                        if self.pairs.len() >= c.max_indices as usize {
                            stats.references_dropped += 1;
                        } else {
                            self.pairs.push((ci, li));
                        }
                    }
                }
            }
        }
        // Counting sort of (cluster, light) pairs into the per-cluster table.
        self.grid.fill([0, 0]);
        for (ci, _) in &self.pairs {
            if let Some(cell) = self.grid.get_mut(*ci as usize) {
                cell[1] += 1;
            }
        }
        let mut offset = 0u32;
        for cell in &mut self.grid {
            cell[0] = offset;
            offset += cell[1];
            cell[1] = 0;
        }
        self.indices.clear();
        self.indices.resize(self.pairs.len(), 0);
        for (ci, li) in &self.pairs {
            if let Some(cell) = self.grid.get_mut(*ci as usize) {
                if let Some(slot) = self.indices.get_mut((cell[0] + cell[1]) as usize) {
                    *slot = *li;
                }
                cell[1] += 1;
            }
        }
        stats.references = u32::try_from(self.pairs.len()).unwrap_or(u32::MAX);
        self.stats = stats;
        stats
    }

    /// Per-cluster `(offset, count)` into [`Clusters::indices`].
    pub fn grid(&self) -> &[[u32; 2]] {
        &self.grid
    }

    /// Light indices, grouped by cluster.
    pub fn indices(&self) -> &[u32] {
        &self.indices
    }

    /// This frame's lights, as uploaded.
    pub fn lights(&self) -> &[GpuPointLight] {
        &self.lights
    }

    /// The lights of cluster `i`.
    pub fn cluster_lights(&self, i: usize) -> &[u32] {
        self.grid
            .get(i)
            .and_then(|[o, n]| self.indices.get(*o as usize..(*o + *n) as usize))
            .unwrap_or(&[])
    }

    /// GPU parameters for a render target of `width x height` pixels.
    #[expect(clippy::cast_precision_loss)] // Pixel and tile counts are small.
    pub fn gpu_params(&self, width: u32, height: u32) -> GpuClusterParams {
        let c = &self.config;
        GpuClusterParams {
            dims: [
                c.tiles_x,
                c.tiles_y,
                c.slices,
                u32::try_from(self.lights.len()).unwrap_or(u32::MAX),
            ],
            depth: [
                c.near,
                c.far,
                c.slices as f32 / (c.far / c.near).ln(),
                width as f32 / c.tiles_x as f32,
            ],
            tile: [height as f32 / c.tiles_y as f32, 0.0, 0.0, 0.0],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn light(x: f32, y: f32, z: f32, range: f32) -> PointLight {
        PointLight {
            position: Vec3::new(x, y, z),
            range,
            color: Vec3::ONE,
            intensity: 1.0,
        }
    }

    #[test]
    fn slices_are_exponential_and_cover_near_to_far() {
        let c = Clusters::new(ClusterConfig::default(), 1.0, 16.0 / 9.0);
        assert!((c.slice_depth(0) - 0.5).abs() < 1e-6);
        assert!((c.slice_depth(24) - 400.0).abs() < 1e-2);
        assert_eq!(c.slice_of(0.1), 0);
        assert_eq!(c.slice_of(10_000.0), 23);
        for k in 0..24 {
            let mid = (c.slice_depth(k) * c.slice_depth(k + 1)).sqrt();
            assert_eq!(c.slice_of(mid), k);
        }
        assert_eq!(c.slice_of(f32::NAN), 0);
    }

    #[test]
    fn assignment_matches_brute_force() {
        let config = ClusterConfig {
            tiles_x: 8,
            tiles_y: 6,
            slices: 12,
            ..ClusterConfig::default()
        };
        let mut c = Clusters::new(config, 1.1, 4.0 / 3.0);
        let view = glam::camera::lh::view::look_at_mat4(
            Vec3::new(3.0, 4.0, 10.0),
            Vec3::new(0.0, 0.0, -20.0),
            Vec3::Y,
        );
        let mut lights = Vec::new();
        for i in 0..64u8 {
            let f = f32::from(i);
            lights.push(light(
                (f * 7.3) % 40.0 - 20.0,
                (f * 3.1) % 10.0,
                -(f * 11.7) % 120.0,
                1.0 + (f % 5.0) * 2.0,
            ));
        }
        lights.push(light(0.0, 0.0, 100.0, 5.0)); // behind the camera
        let stats = c.assign(&view, &lights);
        assert_eq!(
            (stats.lights, stats.lights_dropped, stats.references_dropped),
            (65, 0, 0)
        );
        assert!(stats.references > 64);
        for ci in 0..config.cluster_count() as usize {
            let Some(b) = c.aabb(ci) else { continue };
            let mut expect: Vec<u32> = lights
                .iter()
                .enumerate()
                .filter(|(_, l)| {
                    let center = view.transform_point3(l.position);
                    (center.clamp(b.min, b.max) - center).length_squared() <= l.range * l.range
                })
                .filter_map(|(i, _)| u32::try_from(i).ok())
                .collect();
            let mut got = c.cluster_lights(ci).to_vec();
            expect.sort_unstable();
            got.sort_unstable();
            assert_eq!(got, expect, "cluster {ci}");
        }
    }

    #[test]
    fn capacity_overflow_is_counted_not_grown() {
        let config = ClusterConfig {
            tiles_x: 4,
            tiles_y: 4,
            slices: 4,
            max_lights: 2,
            max_indices: 10,
            ..ClusterConfig::default()
        };
        let mut c = Clusters::new(config, 1.0, 1.0);
        let lights = [
            light(0.0, 0.0, -5.0, 50.0),
            light(0.0, 0.0, -5.0, 50.0),
            light(0.0, 0.0, -5.0, 50.0),
        ];
        let stats = c.assign(&Mat4::IDENTITY, &lights);
        assert_eq!(stats.lights_dropped, 1);
        assert_eq!(stats.references, 10);
        assert!(stats.references_dropped > 0);
        assert!(c.indices.capacity() <= 10 && c.pairs.capacity() <= 10);
    }

    #[test]
    fn invalid_lights_are_ignored() {
        let mut c = Clusters::new(ClusterConfig::default(), 1.0, 1.0);
        let lights = [
            light(f32::NAN, 0.0, -5.0, 1.0),
            light(0.0, 0.0, -5.0, 0.0),
            light(0.0, 0.0, -5.0, -1.0),
        ];
        assert_eq!(c.assign(&Mat4::IDENTITY, &lights).lights, 0);
    }
}
