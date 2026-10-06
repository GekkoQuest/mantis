//! Render-side math: cameras, projections, frustum planes, bounding volumes.
//!
//! Presentation only; nothing here feeds the simulation, so standard float math (and
//! `glam`) is used freely. Conventions: right-handed, +Y up, the camera looks down -Z in
//! view space, `wgpu` clip space (depth in [0, 1]). Projections use **reverse Z with an
//! infinite far plane**: depth 1 at the near plane, approaching 0 at infinity, which gives
//! the best depth precision for large outdoor views.

use glam::{Mat4, Vec3, Vec4};

/// A sphere.
#[derive(Clone, Copy, PartialEq, Debug, Default)]
pub struct Sphere {
    /// Center.
    pub center: Vec3,
    /// Radius.
    pub radius: f32,
}

/// An axis-aligned box.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Aabb {
    /// Minimum corner.
    pub min: Vec3,
    /// Maximum corner.
    pub max: Vec3,
}

impl Aabb {
    /// The bounding sphere of the box.
    pub fn bounding_sphere(&self) -> Sphere {
        let center = (self.min + self.max) * 0.5;
        Sphere {
            center,
            radius: (self.max - center).length(),
        }
    }

    /// Transforms the box and returns the box bounding the result.
    #[must_use]
    pub fn transformed(&self, m: &Mat4) -> Aabb {
        let center = m.transform_point3((self.min + self.max) * 0.5);
        let half = (self.max - self.min) * 0.5;
        let abs = |v: Vec4| Vec3::new(v.x.abs(), v.y.abs(), v.z.abs());
        let extent = abs(m.x_axis) * half.x + abs(m.y_axis) * half.y + abs(m.z_axis) * half.z;
        Aabb {
            min: center - extent,
            max: center + extent,
        }
    }
}

/// A camera: position and orientation plus a projection.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Camera {
    /// World position.
    pub position: Vec3,
    /// Yaw in radians; 0 looks down -Z, positive turns toward +X (right).
    pub yaw: f32,
    /// Pitch in radians; positive looks up.
    pub pitch: f32,
    /// Vertical field of view in radians.
    pub fov_y: f32,
    /// Width over height.
    pub aspect: f32,
    /// Near plane distance.
    pub near: f32,
}

impl Camera {
    /// The direction the camera looks: yaw 0 along +Z, positive yaw toward +X, positive
    /// pitch up (world space, decision 0019, matching `mantis_core::kinematics`).
    pub fn forward(&self) -> Vec3 {
        let (sy, cy) = self.yaw.sin_cos();
        let (sp, cp) = self.pitch.sin_cos();
        Vec3::new(sy * cp, sp, cy * cp)
    }

    /// World to view: left-handed (decision 0019), so view space looks along +Z with +X
    /// right and +Y up. This and [`Camera::projection`] are the renderer's single
    /// world-to-screen boundary; pipelines treat clockwise triangles as front faces.
    pub fn view(&self) -> Mat4 {
        glam::camera::lh::view::look_to_mat4(self.position, self.forward(), Vec3::Y)
    }

    /// View to clip: left-handed, reverse-Z, infinite far plane.
    pub fn projection(&self) -> Mat4 {
        glam::camera::lh::proj::directx::perspective_infinite_reverse(self.fov_y, self.aspect, self.near)
    }

    /// World to clip.
    pub fn view_projection(&self) -> Mat4 {
        self.projection() * self.view()
    }
}

/// A plane `n · p + d = 0`, with `n` unit length and pointing inside.
#[derive(Clone, Copy, PartialEq, Debug, Default)]
pub struct Plane {
    /// Inward unit normal.
    pub normal: Vec3,
    /// Offset.
    pub d: f32,
}

impl Plane {
    fn from_row(v: Vec4) -> Option<Plane> {
        let len = v.truncate().length();
        if len < 1e-12 || !len.is_finite() {
            return None;
        }
        Some(Plane {
            normal: v.truncate() / len,
            d: v.w / len,
        })
    }

    /// Signed distance of `p` (positive inside).
    pub fn distance(&self, p: Vec3) -> f32 {
        self.normal.dot(p) + self.d
    }
}

/// View frustum planes. With an infinite far plane there are five; the far slot is unused.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Frustum {
    /// Left, right, bottom, top, near, far.
    pub planes: [Plane; 6],
    /// How many leading planes are meaningful (5 or 6).
    pub count: usize,
}

impl Frustum {
    /// Extracts the planes of a world-to-clip matrix (Gribb and Hartmann), for `wgpu`
    /// clip space with either depth direction. Degenerate planes (an infinite far plane)
    /// are dropped.
    pub fn from_view_projection(m: &Mat4) -> Frustum {
        let r0 = m.row(0);
        let r1 = m.row(1);
        let r2 = m.row(2);
        let r3 = m.row(3);
        // Depth in [0, 1]: one bound is z >= 0 (row 2), the other z <= w (row 3 - row 2).
        let candidates = [r3 + r0, r3 - r0, r3 + r1, r3 - r1, r2, r3 - r2];
        let mut planes = [Plane::default(); 6];
        let mut count = 0;
        for c in candidates {
            if let Some(p) = Plane::from_row(c)
                && let Some(slot) = planes.get_mut(count)
            {
                *slot = p;
                count += 1;
            }
        }
        Frustum { planes, count }
    }

    /// The meaningful planes.
    pub fn active(&self) -> &[Plane] {
        self.planes.get(..self.count).unwrap_or(&[])
    }

    /// True when the sphere is at least partly inside.
    pub fn intersects_sphere(&self, s: &Sphere) -> bool {
        self.active().iter().all(|p| p.distance(s.center) >= -s.radius)
    }

    /// Classifies a sphere with a safety margin: `Some(true)` clearly inside or crossing,
    /// `Some(false)` clearly outside, `None` within `margin` of a decision boundary (used
    /// to compare GPU and CPU culling without depending on last-bit float agreement).
    pub fn classify_sphere(&self, s: &Sphere, margin: f32) -> Option<bool> {
        let mut inside = true;
        for p in self.active() {
            let d = p.distance(s.center) + s.radius;
            if d.abs() <= margin {
                return None;
            }
            if d < 0.0 {
                inside = false;
            }
        }
        Some(inside)
    }

    /// The planes packed as `vec4`s for GPU upload (unused planes never cull).
    pub fn gpu_planes(&self) -> [[f32; 4]; 6] {
        let mut out = [[0.0, 0.0, 0.0, f32::MAX]; 6];
        for (o, p) in out.iter_mut().zip(self.active()) {
            *o = [p.normal.x, p.normal.y, p.normal.z, p.d];
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn camera() -> Camera {
        Camera {
            position: Vec3::ZERO,
            yaw: 0.0,
            pitch: 0.0,
            fov_y: 1.0,
            aspect: 16.0 / 9.0,
            near: 0.1,
        }
    }

    #[test]
    fn forward_matches_yaw_and_pitch_conventions() {
        let mut c = camera();
        assert!(
            (c.forward() - Vec3::Z).length() < 1e-6,
            "yaw 0 looks along +Z (decision 0019)"
        );
        c.yaw = core::f32::consts::FRAC_PI_2;
        assert!(
            (c.forward() - Vec3::X).length() < 1e-6,
            "positive yaw turns right"
        );
        c.yaw = 0.0;
        c.pitch = core::f32::consts::FRAC_PI_4;
        assert!(c.forward().y > 0.7, "positive pitch looks up");
    }

    #[test]
    fn reverse_z_maps_near_to_one_and_far_toward_zero() {
        let c = camera();
        let vp = c.view_projection();
        let near = vp * Vec4::new(0.0, 0.0, 0.1, 1.0);
        let far = vp * Vec4::new(0.0, 0.0, 1.0e6, 1.0);
        assert!((near.z / near.w - 1.0).abs() < 1e-5);
        assert!(far.z / far.w > 0.0 && far.z / far.w < 1e-6);
    }

    #[test]
    fn infinite_projection_yields_five_planes_and_culls_correctly() {
        let c = camera();
        let f = Frustum::from_view_projection(&c.view_projection());
        assert_eq!(f.count, 5);
        let s = |x: f32, y: f32, z: f32, r: f32| Sphere {
            center: Vec3::new(x, y, z),
            radius: r,
        };
        assert!(f.intersects_sphere(&s(0.0, 0.0, 10.0, 1.0)), "ahead");
        assert!(
            f.intersects_sphere(&s(0.0, 0.0, 1.0e7, 1.0)),
            "far ahead: no far plane"
        );
        assert!(!f.intersects_sphere(&s(0.0, 0.0, -10.0, 1.0)), "behind");
        assert!(!f.intersects_sphere(&s(100.0, 0.0, 10.0, 1.0)), "far right");
        assert!(
            f.intersects_sphere(&s(0.0, 0.0, -0.5, 1.0)),
            "straddles the near plane"
        );
        assert_eq!(f.classify_sphere(&s(0.0, 0.0, 10.0, 1.0), 0.01), Some(true));
        assert_eq!(
            f.classify_sphere(&s(0.0, 0.0, 0.1, 0.0), 0.01),
            None,
            "on the near plane"
        );
    }

    #[test]
    fn finite_orthographic_frustum_has_six_planes() {
        let vp = glam::camera::lh::proj::directx::orthographic(-10.0, 10.0, -10.0, 10.0, 1.0, 100.0);
        let f = Frustum::from_view_projection(&vp);
        assert_eq!(f.count, 6);
        assert!(f.intersects_sphere(&Sphere {
            center: Vec3::new(0.0, 0.0, 50.0),
            radius: 1.0
        }));
        assert!(!f.intersects_sphere(&Sphere {
            center: Vec3::new(0.0, 0.0, 150.0),
            radius: 1.0
        }));
    }

    #[test]
    fn transformed_aabb_bounds_rotated_box() {
        let b = Aabb {
            min: Vec3::splat(-1.0),
            max: Vec3::splat(1.0),
        };
        let m =
            Mat4::from_rotation_y(core::f32::consts::FRAC_PI_4) * Mat4::from_scale(Vec3::new(2.0, 1.0, 1.0));
        let t = b.transformed(&m);
        // Rotating a 4x2 footprint by 45 degrees: half-extent (2 + 1) / sqrt(2) on x and z.
        let e = 3.0 / 2.0f32.sqrt();
        assert!((t.max.x - e).abs() < 1e-5 && (t.max.z - e).abs() < 1e-5 && (t.max.y - 1.0).abs() < 1e-6);
        assert!((b.bounding_sphere().radius - 3.0f32.sqrt()).abs() < 1e-6);
    }
}
