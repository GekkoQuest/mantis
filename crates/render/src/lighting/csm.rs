//! Cascaded shadow maps for the dynamic sun.
//!
//! Split distances use the practical split scheme (a blend of logarithmic and uniform
//! splits). Each cascade is fitted to the bounding sphere of its slice of the view
//! frustum, so its size does not change as the camera rotates, and its origin is snapped
//! to whole shadow-map texels in light space, so shadows do not shimmer as the camera
//! moves. Shadow depth uses a conventional (not reversed) orthographic projection.

use glam::{Mat4, Vec3, Vec4};

use crate::math::Camera;

/// Most cascades.
pub const MAX_CASCADES: usize = 4;

/// Cascade configuration.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct CascadeConfig {
    /// Cascades (1 to [`MAX_CASCADES`]).
    pub count: usize,
    /// Shadow map resolution per cascade.
    pub resolution: u32,
    /// Shadow distance (the last cascade's far split).
    pub max_distance: f32,
    /// Blend between uniform (0) and logarithmic (1) splits.
    pub lambda: f32,
    /// Extra depth toward the sun so casters outside the view still cast.
    pub caster_margin: f32,
}

impl Default for CascadeConfig {
    fn default() -> Self {
        Self {
            count: 4,
            resolution: 2048,
            max_distance: 150.0,
            lambda: 0.8,
            caster_margin: 100.0,
        }
    }
}

/// One fitted cascade.
#[derive(Clone, Copy, PartialEq, Debug, Default)]
pub struct Cascade {
    /// World to shadow clip space.
    pub view_proj: Mat4,
    /// View depth where this cascade ends.
    pub split_far: f32,
    /// World size of one shadow texel.
    pub texel_world: f32,
}

/// Split distances: `count + 1` values from `near` to `far`.
#[allow(clippy::cast_precision_loss)] // Cascade counts are tiny.
pub fn split_distances(near: f32, far: f32, count: usize, lambda: f32) -> [f32; MAX_CASCADES + 1] {
    let n = count.clamp(1, MAX_CASCADES);
    let mut out = [far; MAX_CASCADES + 1];
    for (i, slot) in out.iter_mut().enumerate().take(n + 1) {
        let f = i as f32 / n as f32;
        let log = near * (far / near).powf(f);
        let uni = near + (far - near) * f;
        *slot = lambda * log + (1.0 - lambda) * uni;
    }
    out
}

/// World-space corners of the view frustum between depths `d0` and `d1`.
fn slice_corners(camera: &Camera, d0: f32, d1: f32) -> [Vec3; 8] {
    let inv_view = camera.view().inverse();
    let ky = (camera.fov_y * 0.5).tan();
    let kx = ky * camera.aspect;
    let mut out = [Vec3::ZERO; 8];
    let mut i = 0;
    for d in [d0, d1] {
        for (sx, sy) in [(-1.0, -1.0), (1.0, -1.0), (-1.0, 1.0), (1.0, 1.0)] {
            if let Some(slot) = out.get_mut(i) {
                *slot = inv_view.transform_point3(Vec3::new(sx * kx * d, sy * ky * d, d));
            }
            i += 1;
        }
    }
    out
}

/// Fits every cascade for `camera` and a sun shining along `sun_dir` (from the sun toward
/// the scene).
pub fn fit_cascades(camera: &Camera, sun_dir: Vec3, config: &CascadeConfig) -> [Cascade; MAX_CASCADES] {
    let n = config.count.clamp(1, MAX_CASCADES);
    let splits = split_distances(camera.near, config.max_distance, n, config.lambda);
    let dir = sun_dir.try_normalize().unwrap_or(Vec3::NEG_Y);
    let up = if dir.y.abs() > 0.99 { Vec3::Z } else { Vec3::Y };
    let mut out = [Cascade::default(); MAX_CASCADES];
    for (i, cascade) in out.iter_mut().enumerate().take(n) {
        let (d0, d1) = (
            splits.get(i).copied().unwrap_or(0.0),
            splits.get(i + 1).copied().unwrap_or(0.0),
        );
        let corners = slice_corners(camera, d0, d1);
        let center = corners.iter().fold(Vec3::ZERO, |a, c| a + *c) / 8.0;
        let radius = corners
            .iter()
            .map(|c| (*c - center).length())
            .fold(0.0f32, f32::max);
        // Quantize the radius so the cascade size is stable frame to frame.
        let radius = (radius * 16.0).ceil() / 16.0;
        #[allow(clippy::cast_precision_loss)] // Resolutions are small.
        let texel = 2.0 * radius / config.resolution.max(1) as f32;
        // Light view at the origin, snapped in light space to whole texels.
        let light_view = glam::camera::lh::view::look_to_mat4(Vec3::ZERO, dir, up);
        let c = light_view.transform_point3(center);
        let snapped = Vec3::new((c.x / texel).floor() * texel, (c.y / texel).floor() * texel, c.z);
        let proj = glam::camera::lh::proj::directx::orthographic(
            snapped.x - radius,
            snapped.x + radius,
            snapped.y - radius,
            snapped.y + radius,
            snapped.z - radius - config.caster_margin,
            snapped.z + radius,
        );
        *cascade = Cascade {
            view_proj: proj * light_view,
            split_far: d1,
            texel_world: texel,
        };
    }
    out
}

/// True when `p` projects inside the cascade's clip volume.
pub fn contains(cascade: &Cascade, p: Vec3) -> bool {
    let clip = cascade.view_proj * Vec4::new(p.x, p.y, p.z, 1.0);
    let ndc = clip.truncate() / clip.w;
    ndc.x.abs() <= 1.0 + 1e-4 && ndc.y.abs() <= 1.0 + 1e-4 && (-1e-4..=1.0 + 1e-4).contains(&ndc.z)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn camera(position: Vec3, yaw: f32) -> Camera {
        Camera {
            position,
            yaw,
            pitch: -0.2,
            fov_y: 1.0,
            aspect: 16.0 / 9.0,
            near: 0.1,
        }
    }

    #[test]
    fn splits_blend_log_and_uniform() {
        let s = split_distances(1.0, 100.0, 4, 0.0);
        assert_eq!(s.get(..5), Some(&[1.0, 25.75, 50.5, 75.25, 100.0][..]));
        let s = split_distances(1.0, 100.0, 2, 1.0);
        assert!((s.get(1).copied().unwrap_or(0.0) - 10.0).abs() < 1e-4);
        assert_eq!(s.get(2).copied(), Some(100.0));
    }

    #[test]
    fn every_slice_corner_lies_inside_its_cascade() {
        let config = CascadeConfig::default();
        for yaw in [0.0f32, 0.7, 2.0, -2.5] {
            let cam = camera(Vec3::new(10.0, 3.0, -4.0), yaw);
            let sun = Vec3::new(0.3, -1.0, 0.2);
            let cascades = fit_cascades(&cam, sun, &config);
            let splits = split_distances(cam.near, config.max_distance, config.count, config.lambda);
            for (i, c) in cascades.iter().enumerate().take(config.count) {
                let corners = slice_corners(&cam, splits.get(i).copied().unwrap_or(0.0), c.split_far);
                for p in corners {
                    assert!(contains(c, p), "cascade {i} misses {p:?} at yaw {yaw}");
                }
            }
        }
    }

    #[test]
    fn cascade_size_is_rotation_invariant_and_origin_snaps_to_texels() {
        let config = CascadeConfig::default();
        let sun = Vec3::new(0.3, -1.0, 0.2);
        let a = fit_cascades(&camera(Vec3::ZERO, 0.0), sun, &config);
        let b = fit_cascades(&camera(Vec3::ZERO, 1.3), sun, &config);
        for (ca, cb) in a.iter().zip(&b) {
            assert_eq!(ca.texel_world, cb.texel_world, "same size under rotation");
        }
        // A small camera move shifts the projection only by whole texels: the clip-space
        // position of a fixed world point changes by multiples of one texel (2/resolution).
        let c0 = fit_cascades(&camera(Vec3::new(0.0, 0.0, 0.0), 0.0), sun, &config);
        let c1 = fit_cascades(&camera(Vec3::new(0.013, 0.0, 0.007), 0.0), sun, &config);
        let probe = Vec4::new(2.0, 0.0, -5.0, 1.0);
        for (x, y) in c0.iter().zip(&c1) {
            if x.texel_world != y.texel_world {
                continue;
            }
            let (px, py) = (x.view_proj * probe, y.view_proj * probe);
            let texel_ndc = 2.0 / f32::from(u16::try_from(config.resolution).unwrap_or(u16::MAX));
            for d in [(px.x - py.x) / texel_ndc, (px.y - py.y) / texel_ndc] {
                assert!((d - d.round()).abs() < 1e-2, "shift of {d} texels");
            }
        }
    }
}
