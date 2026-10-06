//! Color gradings: `*.grading.toml` to MGRD (`mantis_formats::color_grading`).
//!
//! Every key is optional and defaults to neutral (no change); each is range-checked at
//! its line.
//!
//! ```toml
//! contrast = 1.1          # 0 to 4, neutral 1
//! saturation = 0.9        # 0 to 4, neutral 1
//! temperature = 0.2       # -1 (blue) to 1 (orange), neutral 0
//! tint = 0.0              # -1 (green) to 1 (magenta), neutral 0
//! lift = [0.02, 0.0, 0.03]   # each -1 to 1, neutral 0
//! gamma = [1.0, 1.0, 0.95]   # each 0.1 to 10, neutral 1
//! gain = [1.05, 1.0, 0.98]   # each 0 to 4, neutral 1
//! ```
//!
//! Output: `<stem>.grd` (`grading/dusk.grading.toml` cooks to `grading/dusk.grd`), kind
//! `ColorGrading`, presentation domain.

use mantis_formats::bundle::{AssetKind, Domain};
use mantis_formats::color_grading::ColorGrading;

use super::fields::{Doc, Fields, output_name, within};
use crate::importer::{CookError, Cooked, ImportContext, Importer, Source};

const SUFFIX: &str = ".grading.toml";

/// The color grading importer.
#[derive(Clone, Copy, Debug, Default)]
pub struct Gradings;

fn scalar(f: &Fields<'_>, key: &str, default: f32, lo: f32, hi: f32) -> Result<f32, CookError> {
    let (v, line) = f.opt_f32(key, default)?;
    if within(v, lo, hi) {
        Ok(v)
    } else {
        Err(f.err(line, &format!("`{key}` = {v} is outside {lo} to {hi}")))
    }
}

fn triple(f: &Fields<'_>, key: &str, default: f32, lo: f32, hi: f32) -> Result<[f32; 3], CookError> {
    let (v, line) = f.opt_array::<3>(key, [default; 3])?;
    match v.iter().find(|c| !within(**c, lo, hi)) {
        Some(c) => Err(f.err(line, &format!("`{key}` component {c} is outside {lo} to {hi}"))),
        None => Ok(v),
    }
}

impl Importer for Gradings {
    fn name(&self) -> &'static str {
        "grading.toml"
    }

    fn version(&self) -> u32 {
        1
    }

    fn phase(&self) -> u32 {
        0
    }

    fn accepts(&self, path: &str) -> bool {
        path.ends_with(SUFFIX)
    }

    fn import(&self, source: &Source<'_>, _ctx: &ImportContext<'_>) -> Result<Vec<Cooked>, CookError> {
        let doc = Doc::parse(source)?;
        doc.only_tables(&[], &[])?;
        let f = doc.root().ok_or_else(|| doc.err(0, "empty document"))?;
        f.only(&[
            "contrast",
            "saturation",
            "temperature",
            "tint",
            "lift",
            "gamma",
            "gain",
        ])?;
        let grading = ColorGrading {
            contrast: scalar(&f, "contrast", 1.0, 0.0, 4.0)?,
            saturation: scalar(&f, "saturation", 1.0, 0.0, 4.0)?,
            temperature: scalar(&f, "temperature", 0.0, -1.0, 1.0)?,
            tint: scalar(&f, "tint", 0.0, -1.0, 1.0)?,
            lift: triple(&f, "lift", 0.0, -1.0, 1.0)?,
            gamma: triple(&f, "gamma", 1.0, 0.1, 10.0)?,
            gain: triple(&f, "gain", 1.0, 0.0, 4.0)?,
        };
        let bytes = grading.encode();
        ColorGrading::parse(&bytes)
            .map_err(|e| doc.err(0, &format!("cooked grading fails its runtime parser: {e}")))?;
        Ok(vec![Cooked {
            name: output_name(source.path, SUFFIX, ".grd"),
            kind: AssetKind::ColorGrading,
            domain: Domain::Presentation,
            bytes,
        }])
    }
}
