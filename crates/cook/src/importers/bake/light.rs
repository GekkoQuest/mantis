//! The light model shared by both bakers.
//!
//! - **Sky**: radiance `sky_color` for directions at or above the horizon (`y >= 0`),
//!   `ground_color` below it, wherever a ray escapes the scene.
//! - **Sun**: a directional light of intensity `sun_color` travelling along
//!   `sun_direction`, visible where a shadow ray toward it escapes.
//! - **Bounce** (`bounces = 1`): a ray that hits the front face of a surface sees that
//!   surface's sunlit diffuse radiance, `albedo * max(0, n . l) * sun_color * visible`,
//!   with one constant albedo for every surface. Sky light reflected by surfaces is not
//!   bounced. Back faces (inside geometry) are black.
//!
//! Quantities are irradiance divided by pi throughout, so `sun_color` is directly the
//! outgoing radiance of a white Lambertian surface facing the sun, as in the renderer.

use super::bvh::Bvh;
use super::math::V3;
use super::settings::{Keyframe, Settings};

/// Ray origins are lifted this far off the surface they leave (meters).
pub const OFFSET: f32 = 2e-3;
/// Nearest accepted hit distance (meters).
pub const T_MIN: f32 = 1e-4;

/// What one ray saw.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Seen {
    /// It escaped along this direction.
    Sky(V3),
    /// It hit a front face at `point` with unit normal `normal`.
    Front {
        /// Hit position.
        point: V3,
        /// Front-face normal.
        normal: V3,
    },
    /// It hit a back face (the origin is inside or behind geometry).
    Back,
}

/// Traces one ray from `origin` along unit `dir`.
pub fn trace(bvh: &Bvh, origin: V3, dir: V3) -> Seen {
    match bvh.closest(origin, dir, T_MIN, f32::INFINITY) {
        None => Seen::Sky(dir),
        Some(h) if h.front => Seen::Front {
            point: origin + dir * h.t,
            normal: h.normal,
        },
        Some(_) => Seen::Back,
    }
}

/// Sky radiance along escaping direction `dir`.
pub fn sky(kf: &Keyframe, dir: V3) -> V3 {
    if dir.y >= 0.0 {
        kf.sky_color
    } else {
        kf.ground_color
    }
}

/// Direct sunlight (irradiance / pi) on a surface at `point` with shading normal `n` and
/// front-face normal `face`.
pub fn sun(bvh: &Bvh, kf: &Keyframe, point: V3, n: V3, face: V3) -> V3 {
    let to_sun = -kf.sun_direction;
    let cos = n.dot(to_sun);
    if cos <= 0.0 || face.dot(to_sun) <= 0.0 || kf.sun_color == V3::ZERO {
        return V3::ZERO;
    }
    if bvh.occluded(point + face * OFFSET, to_sun, T_MIN, f32::INFINITY) {
        return V3::ZERO;
    }
    kf.sun_color * cos
}

/// Radiance arriving along a ray that saw `seen`.
pub fn incoming(bvh: &Bvh, settings: &Settings, kf: &Keyframe, seen: &Seen) -> V3 {
    match seen {
        Seen::Sky(dir) => sky(kf, *dir),
        Seen::Front { point, normal } if settings.bounces > 0 => {
            sun(bvh, kf, *point, *normal, *normal).times(settings.albedo)
        }
        Seen::Front { .. } | Seen::Back => V3::ZERO,
    }
}
