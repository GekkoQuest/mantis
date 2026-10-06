//! Order-1 spherical harmonics as stored in cooked assets, and the evaluation contract.
//!
//! Spatial convention: every position, direction, and transform in this format is in the
//! left-handed world frame of decision 0019 (+X right, +Y up, +Z forward, as
//! `mantis_core::kinematics` defines it); SH basis directions are world-space directions.
//!
//! Coefficients are radiance projections onto the real SH basis
//! `Y00 = 0.282095`, `Y1-1 = 0.488603 y`, `Y10 = 0.488603 z`, `Y11 = 0.488603 x` (y up),
//! stored per color channel as `[L00, L1-1, L10, L11]`. Irradiance for a unit normal applies
//! the clamped-cosine convolution (`A0 = pi`, `A1 = 2 pi / 3`); [`ShL1::irradiance`]
//! returns irradiance divided by pi, the outgoing radiance of a white Lambertian surface,
//! the same quantity lightmaps store. A uniform environment of radiance `c` has
//! `L00 = c * Y00 * 4 pi`.

/// The `Y00` basis constant.
pub const Y0: f32 = 0.282_095;
/// The `Y1m` basis constant.
pub const Y1: f32 = 0.488_603;
/// Clamped-cosine convolution, band 0.
pub const A0: f32 = core::f32::consts::PI;
/// Clamped-cosine convolution, band 1.
pub const A1: f32 = 2.0 * core::f32::consts::PI / 3.0;

/// L1 spherical harmonics for RGB radiance.
#[derive(Clone, Copy, PartialEq, Debug, Default)]
pub struct ShL1 {
    /// Red, green, blue; each `[L00, L1-1, L10, L11]`.
    pub rgb: [[f32; 4]; 3],
}

impl ShL1 {
    /// No light.
    pub const ZERO: ShL1 = ShL1 { rgb: [[0.0; 4]; 3] };

    /// Uniform radiance `color` from every direction.
    pub fn constant(color: [f32; 3]) -> ShL1 {
        let c = |v: f32| [v * Y0 * 4.0 * core::f32::consts::PI, 0.0, 0.0, 0.0];
        let [r, g, b] = color;
        ShL1 {
            rgb: [c(r), c(g), c(b)],
        }
    }

    /// Irradiance divided by pi for unit normal `n = [x, y, z]`, clamped at zero.
    pub fn irradiance(&self, n: [f32; 3]) -> [f32; 3] {
        let [nx, ny, nz] = n;
        self.rgb.map(|[dc, along_y, along_z, along_x]| {
            ((A0 * Y0 * dc + A1 * Y1 * (along_y * ny + along_z * nz + along_x * nx)) / core::f32::consts::PI)
                .max(0.0)
        })
    }

    /// `self * (1 - t) + other * t`.
    #[must_use]
    pub fn lerp(&self, other: &ShL1, t: f32) -> ShL1 {
        let mut out = *self;
        for (o, b) in out.rgb.iter_mut().flatten().zip(other.rgb.iter().flatten()) {
            *o += (*b - *o) * t;
        }
        out
    }

    /// Adds `self * w` into `acc`.
    pub fn accumulate(&self, acc: &mut ShL1, w: f32) {
        for (a, s) in acc.rgb.iter_mut().flatten().zip(self.rgb.iter().flatten()) {
            *a += *s * w;
        }
    }

    /// True when every coefficient is finite.
    pub fn is_finite(&self) -> bool {
        self.rgb.iter().flatten().all(|c| c.is_finite())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constant_irradiance_is_the_color() {
        let sh = ShL1::constant([0.5, 1.0, 2.0]);
        for n in [[1.0, 0.0, 0.0], [0.0, -1.0, 0.0], [0.6, 0.0, 0.8]] {
            let [r, g, b] = sh.irradiance(n);
            assert!((r - 0.5).abs() < 1e-5 && (g - 1.0).abs() < 1e-5 && (b - 2.0).abs() < 1e-5);
        }
    }

    #[test]
    fn lerp_and_accumulate() {
        let a = ShL1::constant([1.0; 3]);
        let m = a.lerp(&ShL1::ZERO, 0.25);
        assert!((m.irradiance([0.0, 1.0, 0.0])[0] - 0.75).abs() < 1e-5);
        let mut acc = ShL1::ZERO;
        a.accumulate(&mut acc, 0.5);
        a.accumulate(&mut acc, 0.5);
        assert!((acc.irradiance([1.0, 0.0, 0.0])[0] - 1.0).abs() < 1e-5);
    }
}
