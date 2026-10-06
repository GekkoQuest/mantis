//! The probe-volume baker.
//!
//! The grid covers the sector's horizontal square and, vertically, from `probe_lift`
//! above the lowest ground sample to `probe_height` above the highest one (from height 0
//! for a sector without ground). Each axis gets `round(extent / probe_spacing) + 1`
//! probes (at least 2), spaced evenly so the first and last sit on the extent's edges.
//!
//! Each probe casts `samples` rays along a fixed Fibonacci sphere ([`super::math`]) and
//! projects the radiance they bring ([`super::light`]: sky where they escape, the sunlit
//! bounce where they hit a front face, black on back faces) onto L1 SH with the
//! convention of `mantis_formats::sh` (`Y00 = 0.282095`, `Y1-1 = 0.488603 y`,
//! `Y10 = 0.488603 z`, `Y11 = 0.488603 x`, each sample weighted by `4 pi / samples`). Direct
//! sunlight is not in the probes: the renderer lights dynamic objects with the dynamic
//! sun. A probe whose rays meet back faces in more than a quarter of the directions is
//! inside geometry: it is marked invalid (and stored as zero) so the renderer's
//! interpolation skips it.

use mantis_formats::probe_volume::{MAX_PROBES, ProbeVolume};
use mantis_formats::sector::SectorInfo;
use mantis_formats::sh::{ShL1, Y0, Y1};

use super::light::{Seen, incoming, trace};
use super::math::{V3, sphere_directions};
use super::scene::Scene;
use super::settings::Settings;

/// Largest magnitude stored (IEEE half precision tops out at 65504).
const SH_LIMIT: f64 = 65_000.0;

/// A probe grid: probes per axis, world position of probe (0, 0, 0), spacing per axis.
pub type Grid = ([u32; 3], [f32; 3], [f32; 3]);

/// The grid: probes per axis, origin, spacing.
///
/// # Errors
/// A message when the grid would exceed the format's probe limit.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss
)]
pub fn grid(info: &SectorInfo, scene: &Scene, settings: &Settings) -> Result<Grid, String> {
    let (lo, hi) = scene.ground_range.unwrap_or((0.0, 0.0));
    let bottom = lo + settings.probe_lift;
    let top = hi + settings.probe_height;
    let size = info.sector_size;
    let extents = [size, top - bottom, size];
    let mut dims = [2u32; 3];
    let mut spacing = [1.0f32; 3];
    for ((d, s), e) in dims.iter_mut().zip(spacing.iter_mut()).zip(extents) {
        let steps = (e / settings.probe_spacing).round().clamp(1.0, 1_048_576.0);
        *d = steps as u32 + 1;
        *s = e / steps;
    }
    let count: u64 = dims.iter().map(|d| u64::from(*d)).product();
    if count > MAX_PROBES {
        return Err(format!(
            "the probe grid would hold {count} probes ({dims:?}); at most {MAX_PROBES}: raise `probe_spacing`"
        ));
    }
    let origin = [info.sector_x as f32 * size, bottom, info.sector_z as f32 * size];
    Ok((dims, origin, spacing))
}

fn project(samples: &[(V3, V3)]) -> ShL1 {
    let count = u32::try_from(samples.len()).unwrap_or(u32::MAX).max(1);
    let weight = 4.0 * core::f64::consts::PI / f64::from(count);
    let mut acc = [[0.0f64; 4]; 3];
    for (dir, radiance) in samples {
        let basis = [
            f64::from(Y0),
            f64::from(Y1 * dir.y),
            f64::from(Y1 * dir.z),
            f64::from(Y1 * dir.x),
        ];
        for (channel, value) in acc.iter_mut().zip(radiance.to_array()) {
            for (c, b) in channel.iter_mut().zip(basis) {
                *c += f64::from(value) * b * weight;
            }
        }
    }
    #[allow(clippy::cast_possible_truncation)] // Clamped to the half range.
    ShL1 {
        rgb: acc.map(|ch| ch.map(|c| c.clamp(-SH_LIMIT, SH_LIMIT) as f32)),
    }
}

/// Bakes the probe volume of a sector.
///
/// # Errors
/// A message when the grid would exceed the format's probe limit.
#[allow(clippy::cast_precision_loss)] // Grid indices are below 2^20.
pub fn bake(info: &SectorInfo, scene: &Scene, settings: &Settings) -> Result<ProbeVolume, String> {
    let (dims, origin, spacing) = grid(info, scene, settings)?;
    let directions = sphere_directions(settings.samples);
    let [across, up, deep] = dims;
    let ([origin_x, origin_y, origin_z], [step_x, step_y, step_z]) = (origin, spacing);
    let count = dims.iter().map(|d| *d as usize).product::<usize>();
    let mut probes: Vec<Vec<ShL1>> = settings
        .keyframes
        .iter()
        .map(|_| Vec::with_capacity(count))
        .collect();
    let mut valid = Vec::with_capacity(count);
    let mut seen = Vec::with_capacity(directions.len());
    let mut samples = Vec::with_capacity(directions.len());
    for z in 0..deep {
        for y in 0..up {
            for x in 0..across {
                let p = V3::new(
                    origin_x + x as f32 * step_x,
                    origin_y + y as f32 * step_y,
                    origin_z + z as f32 * step_z,
                );
                seen.clear();
                seen.extend(directions.iter().map(|d| trace(&scene.bvh, p, *d)));
                let back = seen.iter().filter(|s| matches!(s, Seen::Back)).count();
                let inside = back * 4 > seen.len();
                valid.push(!inside);
                for (frame, kf) in probes.iter_mut().zip(&settings.keyframes) {
                    if inside {
                        frame.push(ShL1::ZERO);
                        continue;
                    }
                    samples.clear();
                    samples.extend(
                        directions
                            .iter()
                            .zip(&seen)
                            .map(|(d, s)| (*d, incoming(&scene.bvh, settings, kf, s))),
                    );
                    frame.push(project(&samples));
                }
            }
        }
    }
    let any_invalid = valid.iter().any(|v| !v);
    Ok(ProbeVolume {
        dims,
        origin,
        spacing,
        keyframes: settings.keyframes.iter().map(|k| k.time).collect(),
        probes,
        valid: any_invalid.then_some(valid),
    })
}
