//! Particle effect v1 (plan 8.5): the data a GPU particle system simulates. An effect is
//! one to eight emitters; spawning the effect starts one emitter instance per emitter.
//!
//! Spatial convention: every position, direction, and transform in this format is in the
//! left-handed world frame of decision 0019 (+X right, +Y up, +Z forward, as
//! `mantis_core::kinematics` defines it); emitter-local axes are that frame rotated by the emitter transform.
//!
//! Conventions: meters, seconds, radians. The cone axis is local +Y. Colors are **straight (not premultiplied) linear RGBA**: RGB
//! may exceed 1 for HDR glow, alpha is coverage in [0, 1]. Sizes are the billboard edge
//! length in meters. Curves are sampled by normalized age (0 at birth, 1 at death) and
//! interpolate linearly between keys, clamping outside the first and last key.
//!
//! Layout (little-endian):
//!
//! | offset | size | field |
//! |---|---|---|
//! | 0 | 4 | magic `"MPFX"` |
//! | 4 | 2 | version `u16` = 1 |
//! | 6 | 2 | flags `u16` = 0 |
//! | 8 | 4 | emitter count `u32`, 1 to 8 |
//! | 12 | 4 | reserved, 0 |
//! | 16 | ... | emitter records, back to back |
//!
//! Emitter record (`60 + 8 B + 20 C + 8 S` bytes, offsets relative to the record):
//!
//! | offset | size | field |
//! |---|---|---|
//! | 0 | 4 | capacity `u32`, 1 to 65536 (live particles of one instance) |
//! | 4 | 4 | duration `f32` seconds, > 0 |
//! | 8 | 4 | rate `f32` particles per second, >= 0 |
//! | 12 | 4 | lifetime min `f32` seconds, > 0 |
//! | 16 | 4 | lifetime max `f32` seconds, >= lifetime min |
//! | 20 | 4 | speed min `f32` m/s |
//! | 24 | 4 | speed max `f32` m/s, >= speed min |
//! | 28 | 12 | acceleration `[f32; 3]` m/s², world axes |
//! | 40 | 4 | drag `f32` per second, >= 0 |
//! | 44 | 1 | shape `u8`: 0 point, 1 sphere, 2 cone |
//! | 45 | 1 | blend `u8`: 0 additive, 1 alpha |
//! | 46 | 1 | space `u8`: 0 world, 1 local |
//! | 47 | 1 | looping `u8`, 0 or 1 |
//! | 48 | 4 | shape a `f32`: sphere radius >= 0, cone half-angle in (0, pi], point 0 |
//! | 52 | 4 | shape b `f32`: cone base radius >= 0, otherwise 0 |
//! | 56 | 1 | burst count B, 0 to 8 |
//! | 57 | 1 | color key count C, 1 to 4 |
//! | 58 | 1 | size key count S, 1 to 4 |
//! | 59 | 1 | reserved, 0 |
//! | 60 | 8 B | bursts: time `f32` in [0, duration), count `u32` in 1 to capacity; sorted by time |
//! | | 20 C | color keys: t `f32` in [0, 1], RGBA `[f32; 4]` (RGB >= 0, alpha in [0, 1]) |
//! | | 8 S | size keys: t `f32` in [0, 1], size `f32` >= 0 |
//!
//! Key times strictly increase. Sizes may be 0 only at the first or last key, and at
//! least one size is positive. An emitter with rate 0 must have a burst. Every float is
//! finite.

use crate::bytes::{FormatError, Reader, Writer};

/// Magic bytes.
pub const MAGIC: [u8; 4] = *b"MPFX";
/// Format version.
pub const VERSION: u16 = 1;
/// Most emitters in one effect.
pub const MAX_EMITTERS: usize = 8;
/// Largest emitter capacity.
pub const MAX_CAPACITY: u32 = 65_536;
/// Most bursts per emitter.
pub const MAX_BURSTS: usize = 8;
/// Most keys per curve.
pub const MAX_KEYS: usize = 4;

/// A burst: `count` particles at `time` seconds into each loop.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Burst {
    /// Seconds from the start of the loop, in [0, duration).
    pub time: f32,
    /// Particles emitted, 1 to the emitter's capacity.
    pub count: u32,
}

/// Where particles are born and which way they leave, in emitter-local axes.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum EmitterShape {
    /// The emitter origin; directions uniform over the sphere.
    Point,
    /// Uniform inside a ball; direction outward from the center.
    Sphere {
        /// Ball radius in meters, >= 0.
        radius: f32,
    },
    /// A disc of `radius` in the local XZ plane; directions within `angle` of local +Y.
    Cone {
        /// Half-angle in radians, in (0, pi].
        angle: f32,
        /// Base disc radius in meters, >= 0.
        radius: f32,
    },
}

/// How particles combine with the scene.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BlendMode {
    /// Light adds (glow, sparks); order-independent.
    Additive,
    /// Coverage blends over (smoke, dust).
    Alpha,
}

/// Which frame particle positions live in.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SimulationSpace {
    /// Particles stay where they were born when the emitter moves.
    World,
    /// Particles move with the emitter transform.
    Local,
}

/// One key of a piecewise-linear curve over normalized age.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct CurveKey<const N: usize> {
    /// Normalized age in [0, 1].
    pub t: f32,
    /// The value at `t`.
    pub value: [f32; N],
}

/// A color key: straight linear RGBA.
pub type ColorKey = CurveKey<4>;
/// A size key: billboard edge length in meters.
pub type SizeKey = CurveKey<1>;

/// One emitter.
#[derive(Clone, PartialEq, Debug)]
pub struct EmitterDef {
    /// Most live particles of one instance, 1 to [`MAX_CAPACITY`].
    pub capacity: u32,
    /// Seconds per loop (or the emission window when not looping), > 0.
    pub duration: f32,
    /// Whether emission restarts after `duration`.
    pub looping: bool,
    /// Continuous emission in particles per second, >= 0.
    pub rate: f32,
    /// Bursts within each loop, sorted by time, at most [`MAX_BURSTS`].
    pub bursts: Vec<Burst>,
    /// Shortest lifetime in seconds, > 0.
    pub lifetime_min: f32,
    /// Longest lifetime in seconds, >= `lifetime_min`.
    pub lifetime_max: f32,
    /// Birth shape.
    pub shape: EmitterShape,
    /// Slowest initial speed in m/s.
    pub speed_min: f32,
    /// Fastest initial speed in m/s, >= `speed_min`.
    pub speed_max: f32,
    /// Constant acceleration in world axes, m/s² (gravity, wind).
    pub acceleration: [f32; 3],
    /// Velocity damping per second, >= 0.
    pub drag: f32,
    /// Color over normalized age, 1 to [`MAX_KEYS`] keys.
    pub color: Vec<ColorKey>,
    /// Size over normalized age, 1 to [`MAX_KEYS`] keys.
    pub size: Vec<SizeKey>,
    /// Blend mode.
    pub blend: BlendMode,
    /// Simulation space.
    pub space: SimulationSpace,
}

/// A particle effect: one to [`MAX_EMITTERS`] emitters.
#[derive(Clone, PartialEq, Debug)]
pub struct ParticleEffect {
    /// The emitters.
    pub emitters: Vec<EmitterDef>,
}

/// Evaluates a piecewise-linear curve at `t`: linear between keys, clamped to the first
/// and last key outside them. An empty curve evaluates to zeros.
pub fn evaluate_curve<const N: usize>(keys: &[CurveKey<N>], t: f32) -> [f32; N] {
    let Some(first) = keys.first() else {
        return [0.0; N];
    };
    // NaN (unordered) routes to the first key too.
    if t.partial_cmp(&first.t) != Some(core::cmp::Ordering::Greater) {
        return first.value;
    }
    for pair in keys.windows(2) {
        let (Some(a), Some(b)) = (pair.first(), pair.get(1)) else {
            continue;
        };
        if t <= b.t {
            let span = b.t - a.t;
            let f = if span > 0.0 { (t - a.t) / span } else { 1.0 };
            let mut out = a.value;
            for (o, (x, y)) in out.iter_mut().zip(a.value.iter().zip(&b.value)) {
                *o = x + (y - x) * f;
            }
            return out;
        }
    }
    keys.last().map_or(first.value, |k| k.value)
}

fn finite(values: &[f32]) -> Result<(), FormatError> {
    if values.iter().all(|v| v.is_finite()) {
        Ok(())
    } else {
        Err(FormatError::NonFinite)
    }
}

fn check(ok: bool, error: FormatError) -> Result<(), FormatError> {
    if ok { Ok(()) } else { Err(error) }
}

fn check_key_times<const N: usize>(keys: &[CurveKey<N>]) -> Result<(), FormatError> {
    check((1..=MAX_KEYS).contains(&keys.len()), FormatError::Keyframes)?;
    for k in keys {
        finite(&[k.t])?;
        finite(&k.value)?;
        check((0.0..=1.0).contains(&k.t), FormatError::Keyframes)?;
    }
    check(
        keys.windows(2)
            .all(|w| matches!((w.first(), w.get(1)), (Some(a), Some(b)) if a.t < b.t)),
        FormatError::Keyframes,
    )
}

impl EmitterShape {
    fn tag(self) -> u8 {
        match self {
            EmitterShape::Point => 0,
            EmitterShape::Sphere { .. } => 1,
            EmitterShape::Cone { .. } => 2,
        }
    }

    fn params(self) -> [f32; 2] {
        match self {
            EmitterShape::Point => [0.0, 0.0],
            EmitterShape::Sphere { radius } => [radius, 0.0],
            EmitterShape::Cone { angle, radius } => [angle, radius],
        }
    }

    fn validate(self) -> Result<(), FormatError> {
        finite(&self.params())?;
        let ok = match self {
            EmitterShape::Point => true,
            EmitterShape::Sphere { radius } => radius >= 0.0,
            EmitterShape::Cone { angle, radius } => {
                angle > 0.0 && angle <= core::f32::consts::PI && radius >= 0.0
            }
        };
        check(ok, FormatError::Dimensions)
    }
}

impl EmitterDef {
    /// Checks every rule in the module documentation.
    ///
    /// # Errors
    /// [`FormatError::NonFinite`] for a non-finite float, [`FormatError::Keyframes`] for
    /// bad key counts, key times, or burst times, [`FormatError::Inconsistent`] for an
    /// emitter that never emits, and [`FormatError::Dimensions`] for any other value out
    /// of range.
    pub fn validate(&self) -> Result<(), FormatError> {
        finite(&[
            self.duration,
            self.rate,
            self.lifetime_min,
            self.lifetime_max,
            self.speed_min,
            self.speed_max,
            self.drag,
        ])?;
        finite(&self.acceleration)?;
        check(
            (1..=MAX_CAPACITY).contains(&self.capacity)
                && self.duration > 0.0
                && self.rate >= 0.0
                && self.lifetime_min > 0.0
                && self.lifetime_min <= self.lifetime_max
                && self.speed_min <= self.speed_max
                && self.drag >= 0.0,
            FormatError::Dimensions,
        )?;
        self.shape.validate()?;
        check(self.bursts.len() <= MAX_BURSTS, FormatError::Dimensions)?;
        let mut previous = 0.0f32;
        for b in &self.bursts {
            finite(&[b.time])?;
            check(
                b.time >= previous && b.time < self.duration,
                FormatError::Keyframes,
            )?;
            check((1..=self.capacity).contains(&b.count), FormatError::Dimensions)?;
            previous = b.time;
        }
        check(
            self.rate > 0.0 || !self.bursts.is_empty(),
            FormatError::Inconsistent,
        )?;
        check_key_times(&self.color)?;
        check(
            self.color.iter().all(|k| {
                let [r, g, b, a] = k.value;
                r >= 0.0 && g >= 0.0 && b >= 0.0 && (0.0..=1.0).contains(&a)
            }),
            FormatError::Dimensions,
        )?;
        check_key_times(&self.size)?;
        let last = self.size.len().saturating_sub(1);
        let interior_positive = self.size.iter().enumerate().all(|(i, k)| {
            if i == 0 || i == last {
                k.value[0] >= 0.0
            } else {
                k.value[0] > 0.0
            }
        });
        check(
            interior_positive && self.size.iter().any(|k| k.value[0] > 0.0),
            FormatError::Dimensions,
        )
    }

    fn parse(r: &mut Reader<'_>) -> Result<EmitterDef, FormatError> {
        let capacity = r.u32()?;
        let duration = r.f32()?;
        let rate = r.f32()?;
        let lifetime_min = r.f32()?;
        let lifetime_max = r.f32()?;
        let speed_min = r.f32()?;
        let speed_max = r.f32()?;
        let acceleration = r.vec3()?;
        let drag = r.f32()?;
        let [shape_tag, blend_tag, space_tag, looping] = r.array::<4>()?;
        let a = r.f32()?;
        let b = r.f32()?;
        let shape = match shape_tag {
            0 => {
                check(a.to_bits() == 0 && b.to_bits() == 0, FormatError::Reserved)?;
                EmitterShape::Point
            }
            1 => {
                check(b.to_bits() == 0, FormatError::Reserved)?;
                EmitterShape::Sphere { radius: a }
            }
            2 => EmitterShape::Cone { angle: a, radius: b },
            other => return Err(FormatError::Encoding(u32::from(other))),
        };
        let blend = match blend_tag {
            0 => BlendMode::Additive,
            1 => BlendMode::Alpha,
            other => return Err(FormatError::Encoding(u32::from(other))),
        };
        let space = match space_tag {
            0 => SimulationSpace::World,
            1 => SimulationSpace::Local,
            other => return Err(FormatError::Encoding(u32::from(other))),
        };
        let looping = match looping {
            0 => false,
            1 => true,
            _ => return Err(FormatError::Validity),
        };
        let [burst_count, color_count, size_count, reserved] = r.array::<4>()?;
        check(reserved == 0, FormatError::Reserved)?;
        check(usize::from(burst_count) <= MAX_BURSTS, FormatError::Dimensions)?;
        let keys_ok = |n: u8| (1..=MAX_KEYS).contains(&usize::from(n));
        check(
            keys_ok(color_count) && keys_ok(size_count),
            FormatError::Keyframes,
        )?;
        let mut bursts = Vec::with_capacity(usize::from(burst_count));
        for _ in 0..burst_count {
            bursts.push(Burst {
                time: r.f32()?,
                count: r.u32()?,
            });
        }
        let mut color = Vec::with_capacity(usize::from(color_count));
        for _ in 0..color_count {
            color.push(ColorKey {
                t: r.f32()?,
                value: [r.f32()?, r.f32()?, r.f32()?, r.f32()?],
            });
        }
        let mut size = Vec::with_capacity(usize::from(size_count));
        for _ in 0..size_count {
            size.push(SizeKey {
                t: r.f32()?,
                value: [r.f32()?],
            });
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
            color,
            size,
            blend,
            space,
        };
        def.validate()?;
        Ok(def)
    }

    fn encode(&self, w: &mut Writer) {
        w.u32(self.capacity);
        for v in [
            self.duration,
            self.rate,
            self.lifetime_min,
            self.lifetime_max,
            self.speed_min,
            self.speed_max,
        ] {
            w.f32(v);
        }
        w.vec3(self.acceleration);
        w.f32(self.drag);
        w.u8(self.shape.tag());
        w.u8(match self.blend {
            BlendMode::Additive => 0,
            BlendMode::Alpha => 1,
        });
        w.u8(match self.space {
            SimulationSpace::World => 0,
            SimulationSpace::Local => 1,
        });
        w.u8(u8::from(self.looping));
        let [a, b] = self.shape.params();
        w.f32(a);
        w.f32(b);
        let small = |n: usize| u8::try_from(n).unwrap_or(u8::MAX);
        w.u8(small(self.bursts.len()));
        w.u8(small(self.color.len()));
        w.u8(small(self.size.len()));
        w.u8(0);
        for burst in &self.bursts {
            w.f32(burst.time);
            w.u32(burst.count);
        }
        for k in &self.color {
            w.f32(k.t);
            for c in k.value {
                w.f32(c);
            }
        }
        for k in &self.size {
            w.f32(k.t);
            w.f32(k.value[0]);
        }
    }
}

impl ParticleEffect {
    /// Checks the emitter count and every emitter.
    ///
    /// # Errors
    /// [`FormatError::Dimensions`] for an emitter count outside 1 to [`MAX_EMITTERS`], or
    /// the first emitter's error (see [`EmitterDef::validate`]).
    pub fn validate(&self) -> Result<(), FormatError> {
        check(
            (1..=MAX_EMITTERS).contains(&self.emitters.len()),
            FormatError::Dimensions,
        )?;
        self.emitters.iter().try_for_each(EmitterDef::validate)
    }

    /// Sum of every emitter's capacity: the pool slots one instance of the effect needs.
    pub fn capacity_total(&self) -> u32 {
        self.emitters
            .iter()
            .fold(0u32, |sum, e| sum.saturating_add(e.capacity))
    }

    /// Parses and validates.
    ///
    /// # Errors
    /// [`FormatError`] describing the first problem found.
    pub fn parse(bytes: &[u8]) -> Result<ParticleEffect, FormatError> {
        let mut r = Reader::new(bytes);
        if r.array::<4>()? != MAGIC {
            return Err(FormatError::Magic);
        }
        let version = r.u16()?;
        if version != VERSION {
            return Err(FormatError::Version(version));
        }
        let flags = r.u16()?;
        if flags != 0 {
            return Err(FormatError::Flags(u32::from(flags)));
        }
        let count = r.u32()?;
        let count = usize::try_from(count).unwrap_or(usize::MAX);
        check((1..=MAX_EMITTERS).contains(&count), FormatError::Dimensions)?;
        check(r.u32()? == 0, FormatError::Reserved)?;
        let mut emitters = Vec::with_capacity(count);
        for _ in 0..count {
            emitters.push(EmitterDef::parse(&mut r)?);
        }
        r.finish()?;
        Ok(ParticleEffect { emitters })
    }

    /// Reference encoder.
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer::new();
        w.bytes(&MAGIC);
        w.u16(VERSION);
        w.u16(0);
        w.count(self.emitters.len());
        w.u32(0);
        for e in &self.emitters {
            e.encode(&mut w);
        }
        w.into_bytes()
    }
}

#[cfg(test)]
mod tests;
