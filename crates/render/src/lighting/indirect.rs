//! Runtime sampling of cooked indirect light (decision 0003). The asset formats themselves
//! live in `mantis-formats` (decision 0016); this module blends keyframes and samples.
//!
//! - Probe volumes: [`blend_probes`] blends the two keyframes around the time of day into
//!   one probe set (cheap, done when the time changes); [`sample_probes`] interpolates it
//!   trilinearly, giving invalid probes zero weight and returning `None` when every
//!   neighbor is invalid so the caller falls back to the sky term.
//! - Lightmaps: [`lightmap_layers`] gives the two layers and weight the shader blends.

use glam::Vec3;
use mantis_formats::lightmap::Lightmap;
use mantis_formats::probe_volume::ProbeVolume;
use mantis_formats::sh::ShL1;
use mantis_formats::time_of_day::keyframe_blend;

/// Blends a volume's probes to `time` (day fraction) into `out`, reusing its storage.
pub fn blend_probes(volume: &ProbeVolume, time: f32, out: &mut Vec<ShL1>) {
    let (a, b, t) = keyframe_blend(&volume.keyframes, time);
    out.clear();
    if let (Some(fa), Some(fb)) = (volume.probes.get(a), volume.probes.get(b)) {
        out.extend(fa.iter().zip(fb).map(|(pa, pb)| pa.lerp(pb, t)));
    }
}

/// Trilinear sample of blended probes at world position `position`, clamped to the volume.
#[expect(clippy::cast_precision_loss)] // Probe counts per axis are far below 2^24.
pub fn sample_probes(volume: &ProbeVolume, blended: &[ShL1], position: Vec3) -> Option<ShL1> {
    let [nx, ny, nz] = volume.dims;
    let last = Vec3::new((nx - 1) as f32, (ny - 1) as f32, (nz - 1) as f32);
    let grid = ((position - Vec3::from(volume.origin)) / Vec3::from(volume.spacing)).clamp(Vec3::ZERO, last);
    let cell = grid.floor().min(last - Vec3::ONE).max(Vec3::ZERO);
    let frac = grid - cell;
    #[expect(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // Clamped above.
    let (cx, cy, cz) = (cell.x as u32, cell.y as u32, cell.z as u32);
    let mut acc = ShL1::ZERO;
    let mut total = 0.0f32;
    for corner in 0..8u32 {
        let (dx, dy, dz) = (corner & 1, (corner >> 1) & 1, (corner >> 2) & 1);
        let weight = (if dx == 1 { frac.x } else { 1.0 - frac.x })
            * (if dy == 1 { frac.y } else { 1.0 - frac.y })
            * (if dz == 1 { frac.z } else { 1.0 - frac.z });
        let index = volume.index(cx + dx, cy + dy, cz + dz);
        let usable = volume
            .valid
            .as_ref()
            .is_none_or(|v| v.get(index).copied().unwrap_or(false));
        if usable
            && weight > 0.0
            && let Some(sh) = blended.get(index)
        {
            sh.accumulate(&mut acc, weight);
            total += weight;
        }
    }
    if total <= 0.0 {
        return None;
    }
    let mut out = ShL1::ZERO;
    acc.accumulate(&mut out, 1.0 / total);
    Some(out)
}

/// The lightmap layers to blend for `time` and the weight of the second.
pub fn lightmap_layers(lightmap: &Lightmap, time: f32) -> (u32, u32, f32) {
    let (a, b, t) = keyframe_blend(&lightmap.keyframes, time);
    (u32::try_from(a).unwrap_or(0), u32::try_from(b).unwrap_or(0), t)
}

#[cfg(test)]
mod tests {
    use super::*;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    fn volume(valid: Option<Vec<bool>>) -> Result<ProbeVolume, mantis_formats::FormatError> {
        let levels = [0.0f32, 1.0, 2.0];
        let k0 = (0..12)
            .map(|i| ShL1::constant([levels.get(i % 3).copied().unwrap_or(0.0); 3]))
            .collect();
        let v = ProbeVolume {
            dims: [3, 2, 2],
            origin: [-1.0, 0.0, 0.0],
            spacing: [1.0, 2.0, 2.0],
            keyframes: vec![0.25, 0.75],
            probes: vec![k0, vec![ShL1::constant([2.0; 3]); 12]],
            valid,
        };
        // Through the cooked format, as at runtime.
        ProbeVolume::parse(&v.encode())
    }

    fn red(sh: &ShL1) -> f32 {
        sh.irradiance([0.0, 1.0, 0.0])[0]
    }

    #[test]
    fn probes_sample_trilinearly_and_blend_in_time() -> TestResult {
        let v = volume(None)?;
        let mut blended = Vec::new();
        blend_probes(&v, 0.25, &mut blended);
        let s = sample_probes(&v, &blended, Vec3::new(-0.5, 1.0, 1.0)).ok_or("sample")?;
        assert!((red(&s) - 0.5).abs() < 2e-3, "halfway between x = 0 and x = 1");
        let s = sample_probes(&v, &blended, Vec3::new(100.0, 0.0, 0.0)).ok_or("sample")?;
        assert!((red(&s) - 2.0).abs() < 2e-3, "clamped outside");
        blend_probes(&v, 0.5, &mut blended);
        let s = sample_probes(&v, &blended, Vec3::new(-1.0, 0.0, 0.0)).ok_or("sample")?;
        assert!((red(&s) - 1.0).abs() < 2e-3, "halfway to the uniform keyframe");
        Ok(())
    }

    #[test]
    fn invalid_probes_are_skipped_and_all_invalid_falls_back() -> TestResult {
        let v = volume(Some((0..12).map(|i| i % 3 != 0).collect()))?;
        let mut blended = Vec::new();
        blend_probes(&v, 0.25, &mut blended);
        let s = sample_probes(&v, &blended, Vec3::new(-0.9, 0.0, 0.0)).ok_or("sample")?;
        assert!(
            (red(&s) - 1.0).abs() < 2e-3,
            "only the valid x = 1 neighbors count"
        );
        let none = volume(Some(vec![false; 12]))?;
        blend_probes(&none, 0.25, &mut blended);
        assert!(sample_probes(&none, &blended, Vec3::ZERO).is_none());
        Ok(())
    }

    #[test]
    fn lightmap_layer_selection_follows_keyframes() {
        let lm = Lightmap {
            width: 1,
            height: 1,
            keyframes: vec![0.0, 0.5],
            layers: vec![vec![0], vec![0]],
        };
        assert_eq!(lightmap_layers(&lm, 0.25), (0, 1, 0.5));
        assert_eq!(lightmap_layers(&lm, 0.75), (1, 0, 0.5));
    }
}
