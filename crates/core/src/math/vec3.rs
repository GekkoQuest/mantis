//! Three-component `f32` vector for simulation state.
//!
//! Conventions: right-handed, `y` up; the ground plane is `x`/`z`.

use core::ops::{Add, AddAssign, Div, Mul, MulAssign, Neg, Sub, SubAssign};

use crate::hash::{StableHasher, StateHash};

/// A three-component `f32` vector. Plain IEEE arithmetic, evaluated in the
/// order written, so results are bit-identical on every target.
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Debug, Default)]
pub struct Vec3 {
    /// X component.
    pub x: f32,
    /// Y component (up).
    pub y: f32,
    /// Z component.
    pub z: f32,
}

impl Vec3 {
    /// The zero vector.
    pub const ZERO: Self = Self::new(0.0, 0.0, 0.0);
    /// Unit X.
    pub const X: Self = Self::new(1.0, 0.0, 0.0);
    /// Unit Y (up).
    pub const Y: Self = Self::new(0.0, 1.0, 0.0);
    /// Unit Z.
    pub const Z: Self = Self::new(0.0, 0.0, 1.0);

    /// A vector from components.
    #[must_use]
    pub const fn new(x: f32, y: f32, z: f32) -> Self {
        Self { x, y, z }
    }

    /// Dot product, evaluated as `(x*x' + y*y') + z*z'`.
    #[must_use]
    pub fn dot(self, o: Self) -> f32 {
        (self.x * o.x + self.y * o.y) + self.z * o.z
    }

    /// Cross product.
    #[must_use]
    pub fn cross(self, o: Self) -> Self {
        Self::new(
            self.y * o.z - self.z * o.y,
            self.z * o.x - self.x * o.z,
            self.x * o.y - self.y * o.x,
        )
    }

    /// Squared length.
    #[must_use]
    pub fn length_squared(self) -> f32 {
        self.dot(self)
    }

    /// Length (IEEE square root of [`Vec3::length_squared`]).
    #[must_use]
    pub fn length(self) -> f32 {
        super::sqrt(self.length_squared())
    }

    /// The unit vector in this direction, or zero when the length is zero or
    /// not finite.
    #[must_use]
    pub fn normalize_or_zero(self) -> Self {
        let len = self.length();
        if len > 0.0 && len.is_finite() {
            self / len
        } else {
            Self::ZERO
        }
    }

    /// Horizontal part (`y` zeroed).
    #[must_use]
    pub const fn horizontal(self) -> Self {
        Self::new(self.x, 0.0, self.z)
    }

    /// Component-wise bit equality: distinguishes `-0.0` from `0.0` and treats
    /// identical NaN bit patterns as equal. Use this, not `==`, to compare
    /// simulation states for determinism.
    #[must_use]
    pub fn bits_eq(self, o: Self) -> bool {
        self.x.to_bits() == o.x.to_bits()
            && self.y.to_bits() == o.y.to_bits()
            && self.z.to_bits() == o.z.to_bits()
    }

    /// True when every component is finite.
    #[must_use]
    pub fn is_finite(self) -> bool {
        self.x.is_finite() && self.y.is_finite() && self.z.is_finite()
    }
}

impl Add for Vec3 {
    type Output = Self;
    fn add(self, o: Self) -> Self {
        Self::new(self.x + o.x, self.y + o.y, self.z + o.z)
    }
}

impl Sub for Vec3 {
    type Output = Self;
    fn sub(self, o: Self) -> Self {
        Self::new(self.x - o.x, self.y - o.y, self.z - o.z)
    }
}

impl Neg for Vec3 {
    type Output = Self;
    fn neg(self) -> Self {
        Self::new(-self.x, -self.y, -self.z)
    }
}

impl Mul<f32> for Vec3 {
    type Output = Self;
    fn mul(self, s: f32) -> Self {
        Self::new(self.x * s, self.y * s, self.z * s)
    }
}

impl Mul<Vec3> for f32 {
    type Output = Vec3;
    fn mul(self, v: Vec3) -> Vec3 {
        v * self
    }
}

impl Div<f32> for Vec3 {
    type Output = Self;
    fn div(self, s: f32) -> Self {
        Self::new(self.x / s, self.y / s, self.z / s)
    }
}

impl AddAssign for Vec3 {
    fn add_assign(&mut self, o: Self) {
        *self = *self + o;
    }
}

impl SubAssign for Vec3 {
    fn sub_assign(&mut self, o: Self) {
        *self = *self - o;
    }
}

impl MulAssign<f32> for Vec3 {
    fn mul_assign(&mut self, s: f32) {
        *self = *self * s;
    }
}

impl StateHash for Vec3 {
    fn state_hash(&self, h: &mut StableHasher) {
        h.write_f32(self.x);
        h.write_f32(self.y);
        h.write_f32(self.z);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arithmetic() {
        let a = Vec3::new(1.0, 2.0, 3.0);
        let b = Vec3::new(4.0, -5.0, 6.0);
        assert_eq!(a + b, Vec3::new(5.0, -3.0, 9.0));
        assert_eq!(a - b, Vec3::new(-3.0, 7.0, -3.0));
        assert_eq!(-a, Vec3::new(-1.0, -2.0, -3.0));
        assert_eq!(a * 2.0, Vec3::new(2.0, 4.0, 6.0));
        assert_eq!(2.0 * a, a * 2.0);
        assert_eq!(b / 2.0, Vec3::new(2.0, -2.5, 3.0));
        assert_eq!(a.dot(b), 12.0);
        assert_eq!(Vec3::X.cross(Vec3::Y), Vec3::Z);
        assert_eq!(Vec3::new(3.0, 0.0, 4.0).length(), 5.0);
        assert_eq!(Vec3::new(3.0, 0.0, 4.0).length_squared(), 25.0);
        assert_eq!(Vec3::new(0.0, 0.0, 2.0).normalize_or_zero(), Vec3::Z);
        assert_eq!(Vec3::ZERO.normalize_or_zero(), Vec3::ZERO);
        assert_eq!(Vec3::new(f32::INFINITY, 0.0, 0.0).normalize_or_zero(), Vec3::ZERO);
        assert_eq!(a.horizontal(), Vec3::new(1.0, 0.0, 3.0));
        let mut c = a;
        c += b;
        c -= b;
        c *= 3.0;
        assert_eq!(c, Vec3::new(3.0, 6.0, 9.0));
    }

    #[test]
    fn bit_equality() {
        assert_eq!(Vec3::new(0.0, 0.0, 0.0), Vec3::new(-0.0, 0.0, 0.0));
        assert!(!Vec3::new(0.0, 0.0, 0.0).bits_eq(Vec3::new(-0.0, 0.0, 0.0)));
        assert!(Vec3::new(1.0, 2.0, 3.0).bits_eq(Vec3::new(1.0, 2.0, 3.0)));
        assert!(!Vec3::new(f32::NAN, 0.0, 0.0).is_finite());
    }
}
