//! Translation, rotation, scale transforms and shortest-path quaternion interpolation.
//!
//! Composition is the usual game-engine TRS convention: `parent * child` composes
//! translations through the parent's rotation and scale, multiplies rotations, and
//! multiplies scales component-wise. It is exact for uniform scale; with non-uniform
//! scale under a rotated child it drops the shear a matrix product would carry, as every
//! TRS hierarchy does. Cooks compute inverse bind matrices with the same convention
//! ([`crate::skeleton::inverse_bind_matrices`]), so the bind pose skins to identity.

use core::ops::Mul;

use glam::{Mat4, Quat, Vec3};

/// Dot products above this interpolate linearly (the arc is too short for `sin`).
const SLERP_LINEAR_DOT: f32 = 0.9995;

/// A local or model-space bone transform.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Transform {
    /// Translation.
    pub translation: Vec3,
    /// Rotation (unit quaternion).
    pub rotation: Quat,
    /// Per-axis scale.
    pub scale: Vec3,
}

impl Default for Transform {
    fn default() -> Self {
        Self::IDENTITY
    }
}

impl Transform {
    /// The identity.
    pub const IDENTITY: Transform = Transform {
        translation: Vec3::ZERO,
        rotation: Quat::IDENTITY,
        scale: Vec3::ONE,
    };

    /// A transform from its parts.
    pub const fn new(translation: Vec3, rotation: Quat, scale: Vec3) -> Self {
        Self {
            translation,
            rotation,
            scale,
        }
    }

    /// A transform from asset arrays (`rotation` is `x, y, z, w` and is renormalized).
    pub fn from_arrays(translation: [f32; 3], rotation: [f32; 4], scale: [f32; 3]) -> Self {
        Self {
            translation: Vec3::from_array(translation),
            rotation: normalize_or_identity(Quat::from_array(rotation)),
            scale: Vec3::from_array(scale),
        }
    }

    /// The inverse, such that `t.inverse() * t` is the identity (exact for uniform scale).
    /// A zero scale component inverts to zero rather than infinity.
    #[must_use]
    pub fn inverse(&self) -> Transform {
        let scale = Vec3::new(
            safe_recip(self.scale.x),
            safe_recip(self.scale.y),
            safe_recip(self.scale.z),
        );
        let rotation = self.rotation.inverse();
        Transform {
            translation: -(scale * (rotation * self.translation)),
            rotation,
            scale,
        }
    }

    /// Applies the transform to a point.
    pub fn transform_point(&self, p: Vec3) -> Vec3 {
        self.translation + self.rotation * (self.scale * p)
    }

    /// The equivalent column-major matrix.
    pub fn to_mat4(&self) -> Mat4 {
        Mat4::from_scale_rotation_translation(self.scale, self.rotation, self.translation)
    }

    /// Interpolates translation and scale linearly and rotation by normalized linear
    /// interpolation along the shortest path. Cheap and order-independent for blending.
    #[must_use]
    pub fn lerp(&self, other: &Transform, t: f32) -> Transform {
        Transform {
            translation: self.translation.lerp(other.translation, t),
            rotation: nlerp_shortest(self.rotation, other.rotation, t),
            scale: self.scale.lerp(other.scale, t),
        }
    }

    /// Like [`Transform::lerp`] but with spherical interpolation of the rotation (constant
    /// angular velocity), for keyframe sampling.
    #[must_use]
    pub fn slerp(&self, other: &Transform, t: f32) -> Transform {
        Transform {
            translation: self.translation.lerp(other.translation, t),
            rotation: slerp_shortest(self.rotation, other.rotation, t),
            scale: self.scale.lerp(other.scale, t),
        }
    }

    /// True when every component is finite.
    pub fn is_finite(&self) -> bool {
        self.translation.is_finite() && self.rotation.is_finite() && self.scale.is_finite()
    }

    /// True when the two transforms agree within `eps` per component (rotations compared
    /// up to sign, since `q` and `-q` are the same rotation).
    pub fn approx_eq(&self, other: &Transform, eps: f32) -> bool {
        self.translation.abs_diff_eq(other.translation, eps)
            && self.scale.abs_diff_eq(other.scale, eps)
            && (self.rotation.abs_diff_eq(other.rotation, eps)
                || self.rotation.abs_diff_eq(-other.rotation, eps))
    }
}

impl Mul for Transform {
    type Output = Transform;

    /// `parent * child`: the child's transform expressed in the parent's space.
    fn mul(self, child: Transform) -> Transform {
        Transform {
            translation: self.transform_point(child.translation),
            rotation: self.rotation * child.rotation,
            scale: self.scale * child.scale,
        }
    }
}

fn safe_recip(v: f32) -> f32 {
    if v.abs() > f32::MIN_POSITIVE {
        v.recip()
    } else {
        0.0
    }
}

/// Normalizes `q`, or returns the identity when it has no usable length.
pub fn normalize_or_identity(q: Quat) -> Quat {
    let len_sq = q.length_squared();
    if len_sq.is_finite() && len_sq > 1e-20 {
        q * len_sq.sqrt().recip()
    } else {
        Quat::IDENTITY
    }
}

/// `b` or `-b`, whichever lies in `a`'s hemisphere (positive dot product).
pub fn align(a: Quat, b: Quat) -> Quat {
    if a.dot(b) < 0.0 { -b } else { b }
}

/// Normalized linear interpolation along the shortest path.
pub fn nlerp_shortest(a: Quat, b: Quat, t: f32) -> Quat {
    let b = align(a, b);
    normalize_or_identity(a * (1.0 - t) + b * t)
}

/// Spherical linear interpolation along the shortest path.
pub fn slerp_shortest(a: Quat, b: Quat, t: f32) -> Quat {
    let mut dot = a.dot(b);
    let b = if dot < 0.0 {
        dot = -dot;
        -b
    } else {
        b
    };
    if dot > SLERP_LINEAR_DOT {
        return normalize_or_identity(a * (1.0 - t) + b * t);
    }
    let theta = dot.clamp(-1.0, 1.0).acos();
    let sin_theta = theta.sin();
    let wa = ((1.0 - t) * theta).sin() / sin_theta;
    let wb = (t * theta).sin() / sin_theta;
    normalize_or_identity(a * wa + b * wb)
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::f32::consts::{FRAC_PI_2, PI};

    fn sample() -> Transform {
        Transform::new(
            Vec3::new(1.0, 2.0, 3.0),
            Quat::from_rotation_y(0.7) * Quat::from_rotation_x(-0.3),
            Vec3::splat(2.0),
        )
    }

    #[test]
    fn composition_matches_matrices() {
        let parent = sample();
        let child = Transform::new(
            Vec3::new(0.5, -1.0, 0.25),
            Quat::from_rotation_z(1.1),
            Vec3::splat(0.5),
        );
        let composed = (parent * child).to_mat4();
        let expected = parent.to_mat4() * child.to_mat4();
        assert!(
            composed.abs_diff_eq(expected, 1e-5),
            "{composed:?} vs {expected:?}"
        );
        let p = Vec3::new(0.3, 0.2, -0.9);
        assert!(
            (parent * child)
                .transform_point(p)
                .abs_diff_eq(parent.transform_point(child.transform_point(p)), 1e-5)
        );
    }

    #[test]
    fn inverse_undoes() {
        let t = sample();
        assert!((t.inverse() * t).approx_eq(&Transform::IDENTITY, 1e-5));
        assert!((t * t.inverse()).approx_eq(&Transform::IDENTITY, 1e-5));
        let zero = Transform::new(Vec3::ZERO, Quat::IDENTITY, Vec3::new(0.0, 1.0, 1.0));
        assert!(zero.inverse().is_finite());
    }

    #[test]
    fn slerp_takes_the_shortest_path() {
        let a = Quat::from_rotation_y(0.0);
        let b = -Quat::from_rotation_y(FRAC_PI_2); // same rotation, opposite hemisphere
        let mid = slerp_shortest(a, b, 0.5);
        assert!(mid.angle_between(Quat::from_rotation_y(FRAC_PI_2 / 2.0)) < 1e-5);
        let nmid = nlerp_shortest(a, b, 0.5);
        assert!(nmid.angle_between(Quat::from_rotation_y(FRAC_PI_2 / 2.0)) < 1e-5);
        // Constant angular velocity: a quarter of the way is a quarter of the angle.
        let wide = Quat::from_rotation_y(PI * 0.9);
        let quarter = slerp_shortest(a, wide, 0.25);
        assert!((quarter.angle_between(a) - PI * 0.9 * 0.25).abs() < 1e-4);
        // Nearly equal rotations fall back to the linear path without NaN.
        let near = slerp_shortest(a, Quat::from_rotation_y(1e-6), 0.5);
        assert!(near.is_finite() && near.is_normalized());
    }

    #[test]
    fn lerp_endpoints() {
        let a = Transform::IDENTITY;
        let b = sample();
        assert!(a.lerp(&b, 0.0).approx_eq(&a, 1e-6));
        assert!(a.lerp(&b, 1.0).approx_eq(&b, 1e-6));
        assert!(a.slerp(&b, 1.0).approx_eq(&b, 1e-6));
        let half = a.lerp(&b, 0.5);
        assert!(half.translation.abs_diff_eq(Vec3::new(0.5, 1.0, 1.5), 1e-6));
        assert!(half.rotation.is_normalized());
    }

    #[test]
    fn from_arrays_normalizes_and_rejects_zero() {
        let t = Transform::from_arrays([0.0; 3], [0.0, 0.0, 0.0, 2.0], [1.0; 3]);
        assert_eq!(t.rotation, Quat::IDENTITY);
        assert_eq!(
            normalize_or_identity(Quat::from_xyzw(0.0, 0.0, 0.0, 0.0)),
            Quat::IDENTITY
        );
        assert_eq!(Transform::default(), Transform::IDENTITY);
    }
}
