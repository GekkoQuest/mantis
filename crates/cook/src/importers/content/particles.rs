//! Particle effects: `*.particles.toml` to MPFX (`mantis_formats::particle_effect`),
//! phase 5 (after leaf assets, before the phase-10 presentation graphs that name effects
//! by source path).
//!
//! One `[emitter.<name>]` table per emitter (1 to 8, in file order). Curve keys and
//! bursts are numbered keys holding arrays, `color.<n> = [t, r, g, b, a]`,
//! `size.<n> = [t, size]`, `burst.<n> = [time, count]`, with `n` counting from 0.
//!
//! ```toml
//! [emitter.sparks]
//! capacity = 256              # 1 to 65536 live particles
//! duration = 1.0              # seconds per loop, > 0
//! looping = false             # default false
//! rate = 0.0                  # particles per second, >= 0 (0 needs a burst)
//! lifetime_min = 0.4          # seconds, > 0
//! lifetime_max = 0.8          # >= lifetime_min
//! speed_min = 2.0             # m/s (default 0)
//! speed_max = 4.0             # >= speed_min (default speed_min)
//! acceleration = [0.0, -9.8, 0.0]   # m/s^2, world axes (default zero)
//! drag = 0.5                  # per second, >= 0 (default 0)
//! shape = "cone"              # point | sphere | cone (default point)
//! angle = 0.6                 # cone half-angle, radians, (0, pi]
//! radius = 0.1                # sphere or cone base radius, >= 0
//! blend = "additive"          # additive | alpha (default additive)
//! space = "world"             # world | local (default world)
//! burst.0 = [0.0, 64]         # at most 8, times ascending in [0, duration)
//! color.0 = [0.0, 1.0, 0.8, 0.3, 1.0]   # 1 to 4 keys, t strictly ascending in [0, 1]
//! color.1 = [1.0, 1.0, 0.2, 0.0, 0.0]   # straight linear RGBA, RGB >= 0, alpha 0 to 1
//! size.0 = [0.0, 0.1]         # 1 to 4 keys; 0 only at the first or last key
//! size.1 = [1.0, 0.0]
//! ```
//!
//! Output: `<stem>.pfx`, kind `ParticleEffect`, presentation domain.

use mantis_formats::bundle::{AssetKind, Domain};
use mantis_formats::particle_effect::{
    BlendMode, Burst, ColorKey, CurveKey, EmitterDef, EmitterShape, MAX_BURSTS, MAX_CAPACITY, MAX_EMITTERS,
    MAX_KEYS, ParticleEffect, SimulationSpace, SizeKey,
};

use super::fields::{Doc, Fields, output_name, within};
use crate::importer::{CookError, Cooked, ImportContext, Importer, Source};

const SUFFIX: &str = ".particles.toml";

/// The particle effect importer.
#[derive(Clone, Copy, Debug, Default)]
pub struct Particles;

const FIXED_KEYS: &[&str] = &[
    "capacity",
    "duration",
    "looping",
    "rate",
    "lifetime_min",
    "lifetime_max",
    "speed_min",
    "speed_max",
    "acceleration",
    "drag",
    "shape",
    "angle",
    "radius",
    "blend",
    "space",
];

/// The numbered keys `<prefix>.0`, `<prefix>.1`, ... as `(values, line)`, checking the
/// numbering is contiguous from 0 and each holds `width` numbers.
fn numbered(f: &Fields<'_>, prefix: &str, width: usize) -> Result<Vec<(Vec<f32>, usize)>, CookError> {
    let mut found: Vec<(u32, usize, String)> = Vec::new();
    for e in f.entries() {
        let Some(rest) = e.key.strip_prefix(prefix).and_then(|r| r.strip_prefix('.')) else {
            continue;
        };
        let n = rest.parse::<u32>().map_err(|_| {
            f.err(
                e.line,
                &format!("`{}`: expected `{prefix}.<n>` with n = 0, 1, ...", e.key),
            )
        })?;
        found.push((n, e.line, e.key.clone()));
    }
    found.sort_unstable();
    let mut out = Vec::with_capacity(found.len());
    for (expected, (n, line, key)) in (0u32..).zip(&found) {
        if *n != expected {
            return Err(f.err(
                *line,
                &format!("`{key}`: `{prefix}` keys must be numbered 0, 1, ... without gaps (expected `{prefix}.{expected}`)"),
            ));
        }
        let (values, line) = f.f32s(key)?;
        if values.len() != width {
            return Err(f.err(
                line,
                &format!("`{key}` must hold {width} numbers, not {}", values.len()),
            ));
        }
        out.push((values, line));
    }
    Ok(out)
}

fn check_keys(f: &Fields<'_>, what: &str, keys: &[(f32, usize)]) -> Result<(), CookError> {
    if keys.is_empty() || keys.len() > MAX_KEYS {
        return Err(f.err(
            f.line,
            &format!("`{what}` needs 1 to {MAX_KEYS} keys, not {}", keys.len()),
        ));
    }
    let mut previous: Option<f32> = None;
    for (t, line) in keys {
        if !within(*t, 0.0, 1.0) {
            return Err(f.err(*line, &format!("`{what}` key time {t} is outside 0 to 1")));
        }
        if previous.is_some_and(|p| *t <= p) {
            return Err(f.err(
                *line,
                &format!(
                    "`{what}` key times must strictly increase ({t} follows {})",
                    previous.unwrap_or(0.0)
                ),
            ));
        }
        previous = Some(*t);
    }
    Ok(())
}

fn colors(f: &Fields<'_>) -> Result<Vec<ColorKey>, CookError> {
    let raw = numbered(f, "color", 5)?;
    let times: Vec<(f32, usize)> = raw
        .iter()
        .map(|(v, l)| (v.first().copied().unwrap_or(0.0), *l))
        .collect();
    check_keys(f, "color", &times)?;
    let mut keys = Vec::with_capacity(raw.len());
    for (v, line) in raw {
        let [t, rgba @ ..] = <[f32; 5]>::try_from(v).map_err(|_| f.err(line, "color key"))?;
        let [red, green, blue, alpha] = rgba;
        if red < 0.0 || green < 0.0 || blue < 0.0 || !within(alpha, 0.0, 1.0) {
            return Err(f.err(
                line,
                "color key: RGB must be >= 0 and alpha within 0 to 1 (straight linear RGBA)",
            ));
        }
        keys.push(CurveKey { t, value: rgba });
    }
    Ok(keys)
}

fn sizes(f: &Fields<'_>) -> Result<Vec<SizeKey>, CookError> {
    let raw = numbered(f, "size", 2)?;
    let times: Vec<(f32, usize)> = raw
        .iter()
        .map(|(v, l)| (v.first().copied().unwrap_or(0.0), *l))
        .collect();
    check_keys(f, "size", &times)?;
    let last = raw.len().saturating_sub(1);
    let mut keys = Vec::with_capacity(raw.len());
    for (i, (v, line)) in raw.into_iter().enumerate() {
        let [t, size] = <[f32; 2]>::try_from(v).map_err(|_| f.err(line, "size key"))?;
        let edge = i == 0 || i == last;
        if size < 0.0 || (!edge && size <= 0.0) {
            return Err(f.err(
                line,
                &format!("size {size}: sizes are > 0, and may be 0 only at the first or last key"),
            ));
        }
        keys.push(CurveKey { t, value: [size] });
    }
    if !keys.iter().any(|k| k.value[0] > 0.0) {
        return Err(f.err(f.line, "every size key is 0: at least one must be positive"));
    }
    Ok(keys)
}

fn bursts(f: &Fields<'_>, duration: f32, capacity: u32) -> Result<Vec<Burst>, CookError> {
    let raw = numbered(f, "burst", 2)?;
    if raw.len() > MAX_BURSTS {
        return Err(f.err(
            f.line,
            &format!("{} bursts; an emitter has at most {MAX_BURSTS}", raw.len()),
        ));
    }
    let mut previous = 0.0f32;
    let mut out = Vec::with_capacity(raw.len());
    for (v, line) in raw {
        let [time, count] = <[f32; 2]>::try_from(v).map_err(|_| f.err(line, "burst"))?;
        if time < previous || time >= duration {
            return Err(f.err(
                line,
                &format!(
                    "burst time {time} must be ascending and within 0 to the duration {duration} (exclusive)"
                ),
            ));
        }
        let whole = count.fract() == 0.0 && (1.0..=65_536.0).contains(&count);
        let count_u = format!("{count}").parse::<u32>().ok().filter(|_| whole);
        let Some(count) = count_u.filter(|c| *c <= capacity) else {
            return Err(f.err(
                line,
                &format!("burst count {count} must be a whole number from 1 to the capacity {capacity}"),
            ));
        };
        previous = time;
        out.push(Burst { time, count });
    }
    Ok(out)
}

fn shape(f: &Fields<'_>) -> Result<EmitterShape, CookError> {
    let options = [("point", 0u8), ("sphere", 1), ("cone", 2)];
    let (tag, _) = f.choice("shape", &options, Some(0))?;
    match tag {
        0 => {
            if let Some(key) = ["angle", "radius"].into_iter().find(|k| f.has(k)) {
                return Err(f.err(f.line_of(key), &format!("a point emitter has no `{key}`")));
            }
            Ok(EmitterShape::Point)
        }
        1 => {
            if f.has("angle") {
                return Err(f.err(f.line_of("angle"), "a sphere emitter has no `angle`"));
            }
            let (radius, line) = f.f32("radius")?;
            if radius < 0.0 {
                return Err(f.err(line, &format!("`radius` = {radius} must be >= 0")));
            }
            Ok(EmitterShape::Sphere { radius })
        }
        _ => {
            let (angle, al) = f.f32("angle")?;
            if !(angle > 0.0 && angle <= core::f32::consts::PI) {
                return Err(f.err(al, &format!("cone `angle` = {angle} must be in (0, pi] radians")));
            }
            let (radius, rl) = f.opt_f32("radius", 0.0)?;
            if radius < 0.0 {
                return Err(f.err(rl, &format!("`radius` = {radius} must be >= 0")));
            }
            Ok(EmitterShape::Cone { angle, radius })
        }
    }
}

fn positive(f: &Fields<'_>, key: &str) -> Result<f32, CookError> {
    let (v, line) = f.f32(key)?;
    if v > 0.0 {
        Ok(v)
    } else {
        Err(f.err(line, &format!("`{key}` = {v} must be > 0")))
    }
}

fn emitter(f: &Fields<'_>) -> Result<EmitterDef, CookError> {
    for e in f.entries() {
        let numbered_key = ["color.", "size.", "burst."].iter().any(|p| e.key.starts_with(p));
        if !numbered_key && !FIXED_KEYS.contains(&e.key.as_str()) {
            return Err(f.err(
                e.line,
                &format!(
                    "unknown key `{}` (expected {}, color.<n>, size.<n>, burst.<n>)",
                    e.key,
                    FIXED_KEYS.join(", ")
                ),
            ));
        }
    }
    let (capacity, cl) = f.int::<u32>("capacity")?;
    if !(1..=MAX_CAPACITY).contains(&capacity) {
        return Err(f.err(
            cl,
            &format!("`capacity` = {capacity} is outside 1 to {MAX_CAPACITY}"),
        ));
    }
    let duration = positive(f, "duration")?;
    let (looping, _) = f.opt_bool("looping", false)?;
    let (rate, rl) = f.opt_f32("rate", 0.0)?;
    if rate < 0.0 {
        return Err(f.err(rl, &format!("`rate` = {rate} must be >= 0")));
    }
    let lifetime_min = positive(f, "lifetime_min")?;
    let (lifetime_max, ll) = f.f32("lifetime_max")?;
    if lifetime_max < lifetime_min {
        return Err(f.err(
            ll,
            &format!("`lifetime_max` = {lifetime_max} is below `lifetime_min` = {lifetime_min}"),
        ));
    }
    let (speed_min, _) = f.opt_f32("speed_min", 0.0)?;
    let (speed_max, sl) = f.opt_f32("speed_max", speed_min)?;
    if speed_max < speed_min {
        return Err(f.err(
            sl,
            &format!("`speed_max` = {speed_max} is below `speed_min` = {speed_min}"),
        ));
    }
    let (acceleration, _) = f.opt_array::<3>("acceleration", [0.0; 3])?;
    let (drag, dl) = f.opt_f32("drag", 0.0)?;
    if drag < 0.0 {
        return Err(f.err(dl, &format!("`drag` = {drag} must be >= 0")));
    }
    let shape = shape(f)?;
    let (blend, _) = f.choice(
        "blend",
        &[("additive", BlendMode::Additive), ("alpha", BlendMode::Alpha)],
        Some(BlendMode::Additive),
    )?;
    let (space, _) = f.choice(
        "space",
        &[
            ("world", SimulationSpace::World),
            ("local", SimulationSpace::Local),
        ],
        Some(SimulationSpace::World),
    )?;
    let bursts = bursts(f, duration, capacity)?;
    if rate == 0.0 && bursts.is_empty() {
        return Err(f.err(
            f.line,
            "the emitter never emits: give it a `rate` > 0 or a `burst.0`",
        ));
    }
    let def = EmitterDef {
        capacity,
        duration,
        looping,
        rate,
        bursts,
        lifetime_min,
        lifetime_max,
        shape,
        speed_min,
        speed_max,
        acceleration,
        drag,
        color: colors(f)?,
        size: sizes(f)?,
        blend,
        space,
    };
    def.validate()
        .map_err(|e| f.err(f.line, &format!("emitter fails validation: {e}")))?;
    Ok(def)
}

impl Importer for Particles {
    fn name(&self) -> &'static str {
        "particles.toml"
    }

    fn version(&self) -> u32 {
        1
    }

    fn phase(&self) -> u32 {
        5
    }

    fn accepts(&self, path: &str) -> bool {
        path.ends_with(SUFFIX)
    }

    fn import(&self, source: &Source<'_>, _ctx: &ImportContext<'_>) -> Result<Vec<Cooked>, CookError> {
        let doc = Doc::parse(source)?;
        doc.only_tables(&[], &["emitter."])?;
        if let Some(root) = doc.root() {
            root.only(&[])?;
        }
        let mut emitters = Vec::new();
        let mut last_line = 0;
        for (_, f) in doc.prefixed("emitter.") {
            emitters.push(emitter(&f)?);
            last_line = f.line;
        }
        if emitters.is_empty() || emitters.len() > MAX_EMITTERS {
            return Err(doc.err(
                last_line,
                &format!("{} emitters; an effect has 1 to {MAX_EMITTERS}", emitters.len()),
            ));
        }
        let effect = ParticleEffect { emitters };
        let bytes = effect.encode();
        ParticleEffect::parse(&bytes)
            .map_err(|e| doc.err(0, &format!("cooked effect fails its runtime parser: {e}")))?;
        Ok(vec![Cooked {
            name: output_name(source.path, SUFFIX, ".pfx"),
            kind: AssetKind::ParticleEffect,
            domain: Domain::Presentation,
            bytes,
        }])
    }
}
