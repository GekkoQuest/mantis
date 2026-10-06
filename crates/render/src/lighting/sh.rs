//! Spherical harmonics on the render side: projecting environments (the sky) into the
//! L1 representation that cooked probes use. The representation and its evaluation
//! contract are `mantis_formats::sh`'s.

use glam::Vec3;
pub use mantis_formats::sh::ShL1;
use mantis_formats::sh::{Y0, Y1};

/// Adds radiance `color` arriving from unit direction `dir`, weighted by `weight` (a
/// solid-angle weight when projecting a sampled environment).
pub fn add_sample(sh: &mut ShL1, dir: Vec3, color: Vec3, weight: f32) {
    let basis = [Y0, Y1 * dir.y, Y1 * dir.z, Y1 * dir.x];
    for (channel, value) in sh.rgb.iter_mut().zip([color.x, color.y, color.z]) {
        for (c, b) in channel.iter_mut().zip(basis) {
            *c += value * b * weight;
        }
    }
}

/// Irradiance divided by pi for unit normal `n`.
pub fn irradiance(sh: &ShL1, n: Vec3) -> Vec3 {
    Vec3::from(sh.irradiance(n.to_array()))
}

/// Projects a radiance function over the sphere with a deterministic Fibonacci sample set.
#[expect(clippy::cast_precision_loss)] // Sample counts are far below 2^24.
pub fn project(samples: u32, mut radiance: impl FnMut(Vec3) -> Vec3) -> ShL1 {
    let count = samples.max(1);
    let weight = 4.0 * core::f32::consts::PI / count as f32;
    let golden = core::f32::consts::PI * (3.0 - 5.0f32.sqrt());
    let mut sh = ShL1::ZERO;
    for i in 0..count {
        let height = 1.0 - (i as f32 + 0.5) / count as f32 * 2.0;
        let ring = (1.0 - height * height).max(0.0).sqrt();
        let (sin, cos) = (golden * i as f32).sin_cos();
        let dir = Vec3::new(cos * ring, height, sin * ring);
        add_sample(&mut sh, dir, radiance(dir), weight);
    }
    sh
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn projection_agrees_with_the_cooked_constant_convention() {
        let projected = project(4096, |_| Vec3::new(0.5, 1.0, 2.0));
        let cooked = ShL1::constant([0.5, 1.0, 2.0]);
        for n in [Vec3::X, Vec3::NEG_Y, Vec3::new(0.6, 0.0, 0.8)] {
            assert!((irradiance(&projected, n) - irradiance(&cooked, n)).length() < 1e-2);
        }
    }

    #[test]
    fn light_from_above_brightens_upward_normals() {
        let sh = project(4096, |d| if d.y > 0.0 { Vec3::ONE } else { Vec3::ZERO });
        let up = irradiance(&sh, Vec3::Y).x;
        let side = irradiance(&sh, Vec3::X).x;
        let down = irradiance(&sh, Vec3::NEG_Y).x;
        assert!(up > side && side > down, "{up} {side} {down}");
        // A unit-radiance upper hemisphere: exact values are 1, 0.5, 0; L1 is close.
        assert!((side - 0.5).abs() < 0.02);
        assert!(up > 0.85 && down < 0.15);
    }
}
