//! The `[bake]` and `[bake.<keyframe>]` tables of a sector source.

use mantis_formats::lightmap::MAX_EDGE;
use mantis_formats::time_of_day::MAX_KEYFRAMES;

use super::math::V3;
use crate::importer::CookError;
use crate::source::{Doc, Fields};

/// One time-of-day keyframe.
#[derive(Clone, Debug, PartialEq)]
pub struct Keyframe {
    /// Table name (`[bake.<name>]`).
    pub name: String,
    /// Day fraction in [0, 1).
    pub time: f32,
    /// Unit direction the sunlight travels (from the sun toward the ground), as the
    /// renderer's frame `sun_direction`.
    pub sun_direction: V3,
    /// Sun intensity: the outgoing radiance of a white Lambertian surface facing the sun
    /// (the renderer's `sun_color`).
    pub sun_color: V3,
    /// Sky radiance for directions at or above the horizon.
    pub sky_color: V3,
    /// Radiance for directions below the horizon that hit no geometry.
    pub ground_color: V3,
}

/// Every bake setting of one sector.
#[derive(Clone, Debug, PartialEq)]
pub struct Settings {
    /// Target distance between probes on every axis (meters).
    pub probe_spacing: f32,
    /// Source line of `probe_spacing` (or of the `[bake]` table), for volume errors.
    pub probe_spacing_line: usize,
    /// How far above the highest ground sample the probe volume reaches (meters).
    pub probe_height: f32,
    /// Height of the lowest probe layer above the lowest ground sample (meters).
    pub probe_lift: f32,
    /// Lightmap density (texels per meter of surface edge).
    pub texels_per_meter: f32,
    /// Largest atlas edge (texels).
    pub max_size: u32,
    /// Empty texels around every rectangle, filled by dilation.
    pub padding: u32,
    /// Rays per probe and per lightmap texel.
    pub samples: u32,
    /// Indirect bounces, 0 or 1.
    pub bounces: u32,
    /// Constant diffuse albedo of every surface for the bounce.
    pub albedo: V3,
    /// Whether lightmaps include direct sunlight.
    pub lightmap_sun: bool,
    /// Keyframes, ordered by time.
    pub keyframes: Vec<Keyframe>,
}

fn color(f: &Fields<'_>, key: &str, default: V3) -> Result<V3, CookError> {
    if !f.has(key) {
        return Ok(default);
    }
    let c = f.floats::<3>(key)?;
    if c.iter().any(|v| *v < 0.0) {
        return Err(f.error(key, &format!("`{key}` must not be negative")));
    }
    Ok(V3::from_array(c))
}

fn positive(f: &Fields<'_>, key: &str, default: f32) -> Result<f32, CookError> {
    let v = f.float_or(key, default)?;
    if v <= 0.0 {
        return Err(f.error(key, &format!("`{key}` must be positive")));
    }
    Ok(v)
}

fn int_or(f: &Fields<'_>, key: &str, lo: i64, hi: i64, default: u32) -> Result<u32, CookError> {
    if !f.has(key) {
        return Ok(default);
    }
    let v = f.int(key, lo, hi)?;
    u32::try_from(v).map_err(|_| f.error(key, &format!("`{key}` is out of range")))
}

const BAKE_KEYS: [&str; 12] = [
    "probe_spacing",
    "probe_height",
    "probe_lift",
    "lightmap_texels_per_meter",
    "lightmap_max_size",
    "lightmap_padding",
    "lightmap_sun",
    "samples",
    "bounces",
    "albedo",
    "sky_color",
    "ground_color",
];

fn keyframe(name: &str, f: &Fields<'_>, sky: V3, ground: V3) -> Result<Keyframe, CookError> {
    f.only(&["time", "sun_direction", "sun_color", "sky_color", "ground_color"])?;
    let time = f.float("time")?;
    if !(0.0..1.0).contains(&time) {
        return Err(f.error("time", "`time` is a day fraction from 0 up to (not including) 1"));
    }
    let dir = V3::from_array(f.floats::<3>("sun_direction")?);
    if dir.length() < 1e-6 {
        return Err(f.error("sun_direction", "`sun_direction` must not be zero"));
    }
    if !f.has("sun_color") {
        return Err(f.error("sun_color", "missing `sun_color`"));
    }
    Ok(Keyframe {
        name: name.to_owned(),
        time,
        sun_direction: dir.normalized(),
        sun_color: color(f, "sun_color", V3::ZERO)?,
        sky_color: color(f, "sky_color", sky)?,
        ground_color: color(f, "ground_color", ground)?,
    })
}

/// Reads the bake settings of the sector source `text` at `path`: `None` when it has no
/// `[bake]` table (or does not parse at all: the sector importer reports syntax errors).
///
/// # Errors
/// [`CookError`] at the offending key.
pub fn parse_settings(path: &str, text: &str) -> Result<Option<Settings>, CookError> {
    let Ok(doc) = Doc::parse(path, text) else {
        return Ok(None);
    };
    let items = doc.items("bake");
    let Some(f) = doc.table("bake") else {
        return match items.first() {
            Some((_, k)) => Err(k.error("time", "keyframe tables need a [bake] table")),
            None => Ok(None),
        };
    };
    f.only(&BAKE_KEYS)?;
    let probe_spacing = positive(&f, "probe_spacing", 4.0)?;
    let probe_height = positive(&f, "probe_height", 8.0)?;
    let probe_lift = f.float_or("probe_lift", 0.5)?;
    if probe_lift < 0.0 || probe_lift >= probe_height {
        return Err(f.error("probe_lift", "`probe_lift` must be from 0 up to `probe_height`"));
    }
    let texels_per_meter = positive(&f, "lightmap_texels_per_meter", 4.0)?;
    let max_size = int_or(&f, "lightmap_max_size", 16, i64::from(MAX_EDGE), 1024)?;
    let padding = int_or(&f, "lightmap_padding", 1, 8, 2)?;
    let samples = int_or(&f, "samples", 16, 16_384, 256)?;
    let bounces = int_or(&f, "bounces", 0, 1, 1)?;
    let albedo = color(&f, "albedo", V3::new(0.5, 0.5, 0.5))?;
    if albedo.x > 1.0 || albedo.y > 1.0 || albedo.z > 1.0 {
        return Err(f.error("albedo", "`albedo` components are from 0 to 1"));
    }
    let lightmap_sun = f.bool_or("lightmap_sun", true)?;
    let sky = color(&f, "sky_color", V3::new(1.0, 1.0, 1.0))?;
    let ground = color(&f, "ground_color", V3::ZERO)?;
    let mut keyframes = Vec::new();
    for (name, k) in &items {
        if keyframes.len() == MAX_KEYFRAMES {
            return Err(k.error("time", &format!("at most {MAX_KEYFRAMES} keyframes")));
        }
        let kf = keyframe(name, k, sky, ground)?;
        if keyframes
            .iter()
            .any(|o: &Keyframe| o.time.to_bits() == kf.time.to_bits())
        {
            return Err(k.error("time", "two keyframes share this `time`"));
        }
        keyframes.push(kf);
    }
    if keyframes.is_empty() {
        return Err(f.error(
            "samples",
            "a bake needs at least one keyframe table ([bake.<name>] with `time`, `sun_direction`, `sun_color`)",
        ));
    }
    keyframes.sort_by(|a, b| a.time.total_cmp(&b.time));
    Ok(Some(Settings {
        probe_spacing,
        probe_spacing_line: f.line("probe_spacing"),
        probe_height,
        probe_lift,
        texels_per_meter,
        max_size,
        padding,
        samples,
        bounces,
        albedo,
        lightmap_sun,
        keyframes,
    }))
}
