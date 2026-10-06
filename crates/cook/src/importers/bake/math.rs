//! Small vector math and the fixed direction sets of the bakers.
//!
//! Everything here uses only IEEE-exact operations (`+`, `-`, `*`, `/`, `sqrt`) so a bake
//! produces the same bytes on every machine: the direction sets are generated with a
//! rotation recurrence from hard-coded golden-angle constants instead of `sin`/`cos`.

use core::ops::{Add, Mul, Neg, Sub};

/// A 3D vector (world space, decision 0019: +X right, +Y up, +Z forward).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct V3 {
    /// X.
    pub x: f32,
    /// Y.
    pub y: f32,
    /// Z.
    pub z: f32,
}

impl V3 {
    /// The zero vector.
    pub const ZERO: V3 = V3 {
        x: 0.0,
        y: 0.0,
        z: 0.0,
    };

    /// A vector from components.
    pub const fn new(x: f32, y: f32, z: f32) -> V3 {
        V3 { x, y, z }
    }

    /// A vector from an array.
    pub const fn from_array(a: [f32; 3]) -> V3 {
        let [x, y, z] = a;
        V3 { x, y, z }
    }

    /// The components as an array.
    pub const fn to_array(self) -> [f32; 3] {
        [self.x, self.y, self.z]
    }

    /// Dot product.
    pub fn dot(self, o: V3) -> f32 {
        self.x * o.x + self.y * o.y + self.z * o.z
    }

    /// Cross product (`(b - a) x (c - a)` is a triangle's outward normal, decision 0019).
    #[must_use]
    pub fn cross(self, o: V3) -> V3 {
        V3::new(
            self.y * o.z - self.z * o.y,
            self.z * o.x - self.x * o.z,
            self.x * o.y - self.y * o.x,
        )
    }

    /// Length.
    pub fn length(self) -> f32 {
        self.dot(self).sqrt()
    }

    /// Unit vector, or zero for a (near) zero vector.
    #[must_use]
    pub fn normalized(self) -> V3 {
        let len = self.length();
        if len > 1e-20 { self * (1.0 / len) } else { V3::ZERO }
    }

    /// Component-wise minimum.
    #[must_use]
    pub fn min(self, o: V3) -> V3 {
        V3::new(self.x.min(o.x), self.y.min(o.y), self.z.min(o.z))
    }

    /// Component-wise maximum.
    #[must_use]
    pub fn max(self, o: V3) -> V3 {
        V3::new(self.x.max(o.x), self.y.max(o.y), self.z.max(o.z))
    }

    /// Component-wise product.
    #[must_use]
    pub fn times(self, o: V3) -> V3 {
        V3::new(self.x * o.x, self.y * o.y, self.z * o.z)
    }

    /// Component `axis` (0 x, 1 y, otherwise z).
    pub fn axis(self, axis: usize) -> f32 {
        match axis {
            0 => self.x,
            1 => self.y,
            _ => self.z,
        }
    }
}

impl Add for V3 {
    type Output = V3;
    fn add(self, o: V3) -> V3 {
        V3::new(self.x + o.x, self.y + o.y, self.z + o.z)
    }
}

impl Sub for V3 {
    type Output = V3;
    fn sub(self, o: V3) -> V3 {
        V3::new(self.x - o.x, self.y - o.y, self.z - o.z)
    }
}

impl Mul<f32> for V3 {
    type Output = V3;
    fn mul(self, s: f32) -> V3 {
        V3::new(self.x * s, self.y * s, self.z * s)
    }
}

impl Neg for V3 {
    type Output = V3;
    fn neg(self) -> V3 {
        V3::new(-self.x, -self.y, -self.z)
    }
}

/// `cos` and `sin` of the golden angle `pi * (3 - sqrt 5)`.
const GOLDEN_COS: f64 = -0.737_368_878_078_319_7;
const GOLDEN_SIN: f64 = 0.675_490_294_261_523_8;

/// `(cos, sin)` of `i * golden angle` for `i` in `0..count`, by exact-operation recurrence.
fn golden_turns(count: u32) -> Vec<(f64, f64)> {
    let mut out = Vec::with_capacity(count as usize);
    let (mut c, mut s) = (1.0f64, 0.0f64);
    for _ in 0..count {
        out.push((c, s));
        let next = (c * GOLDEN_COS - s * GOLDEN_SIN, s * GOLDEN_COS + c * GOLDEN_SIN);
        // Renormalize so rounding never accumulates into the radius.
        let len = (next.0 * next.0 + next.1 * next.1).sqrt();
        (c, s) = (next.0 / len, next.1 / len);
    }
    out
}

/// `count` unit directions evenly spread over the sphere (a Fibonacci sphere, the same
/// construction as the renderer's sky projection): direction `i` has height
/// `1 - (i + 0.5) / count * 2` along +Y and turns by the golden angle around it. Each
/// direction stands for a solid angle of `4 pi / count`.
#[expect(clippy::cast_possible_truncation)] // Unit components fit f32.
pub fn sphere_directions(count: u32) -> Vec<V3> {
    let n = f64::from(count.max(1));
    golden_turns(count.max(1))
        .into_iter()
        .zip(0u32..)
        .map(|((c, s), i)| {
            let height = 1.0 - (f64::from(i) + 0.5) / n * 2.0;
            let ring = (1.0 - height * height).max(0.0).sqrt();
            V3::new((c * ring) as f32, height as f32, (s * ring) as f32)
        })
        .collect()
}

/// `count` directions on the hemisphere around local +Y, distributed by `cos theta` (a
/// Fibonacci spiral on the unit disk lifted to the hemisphere): the plain average of a
/// radiance over them estimates irradiance divided by pi.
#[expect(clippy::cast_possible_truncation)] // Unit components fit f32.
pub fn cosine_directions(count: u32) -> Vec<V3> {
    let n = f64::from(count.max(1));
    golden_turns(count.max(1))
        .into_iter()
        .zip(0u32..)
        .map(|((c, s), i)| {
            let r2 = (f64::from(i) + 0.5) / n;
            let r = r2.sqrt();
            V3::new((c * r) as f32, (1.0 - r2).max(0.0).sqrt() as f32, (s * r) as f32)
        })
        .collect()
}

/// Two unit vectors completing unit `n` to an orthonormal frame (`t`, `n`, `b`).
pub fn frame(n: V3) -> (V3, V3) {
    // Duff et al., "Building an orthonormal basis, revisited", around n.
    let sign = if n.z >= 0.0 { 1.0 } else { -1.0 };
    let a = -1.0 / (sign + n.z);
    let b = n.x * n.y * a;
    let t = V3::new(1.0 + sign * n.x * n.x * a, sign * b, -sign * n.x);
    let bt = V3::new(b, sign + n.y * n.y * a, -n.y);
    (t, bt)
}

/// Rotates local direction `d` (around +Y) into the frame whose up axis is `n`.
pub fn to_frame(d: V3, n: V3, t: V3, b: V3) -> V3 {
    t * d.x + n * d.y + b * d.z
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn direction_sets_are_unit_and_balanced() {
        let sphere = sphere_directions(512);
        assert!(sphere.iter().all(|d| (d.length() - 1.0).abs() < 1e-5));
        let sum = sphere.iter().fold(V3::ZERO, |acc, d| acc + *d);
        assert!(sum.length() < 1e-2 * 512.0, "{sum:?}");
        let hemi = cosine_directions(512);
        assert!(hemi.iter().all(|d| (d.length() - 1.0).abs() < 1e-5 && d.y > 0.0));
        let up = V3::new(0.3, -0.5, 0.8).normalized();
        let (tangent, bitangent) = frame(up);
        assert!(tangent.dot(up).abs() < 1e-5 && bitangent.dot(up).abs() < 1e-5);
        assert!(tangent.dot(bitangent).abs() < 1e-5);
        assert!((tangent.length() - 1.0).abs() < 1e-5 && (bitangent.length() - 1.0).abs() < 1e-5);
    }
}
