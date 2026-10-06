//! The package's default tunables, read from `package.toml`.
//!
//! Only the `[tunables.*]` tables are read here, with a deliberately small
//! grammar: `key = number   # unit`. Every key is required, unknown and
//! duplicate keys are refused, and **every line must document its unit** in
//! the trailing comment, so no tunable ships without one.

use core::fmt;

use mantis_core::content::ContentHash;
use mantis_core::kinematics::MotionParams;
use mantis_core::time::TickRate;
use mantis_server::interest::TierConfig;
use mantis_server::limits::RateLimits;
use mantis_server::movement::EnvelopeConfig;

/// The manifest compiled into the server (the defaults).
pub const PACKAGE_TOML: &str = include_str!("../../package.toml");

/// Everything the package tunes.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Tunables {
    /// Simulation rate.
    pub tick_rate: TickRate,
    /// Movement of the class.
    pub motion: MotionParams,
    /// Validated-mode tolerances.
    pub envelope: EnvelopeConfig,
    /// Replication interest.
    pub interest: TierConfig,
    /// Per-session rate limits (plan 12).
    pub limits: RateLimits,
    /// How long an instance cell stays empty before it is released to the
    /// realm, in seconds (plan 7.1; the engine applies the rule).
    pub instance_release_grace: u32,
    /// The content hash clients must match: the hash of the cooked, signed
    /// gameplay bundle ([`crate::world::cooked_content`]). Until a cook is
    /// read it is the hash of the manifest text, which tests and tools that
    /// do not cook (and clients that announce the manifest hash) use.
    pub content: ContentHash,
}

/// A manifest was refused.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum TunableError {
    /// A line inside a tunables table is not `key = value # unit`.
    Syntax(usize),
    /// A key no tunable has.
    Unknown(String),
    /// A key given twice.
    Duplicate(String),
    /// A required key is absent.
    Missing(&'static str),
    /// A line without a unit comment.
    MissingUnit(String),
    /// A value that is not a finite number of the right kind.
    BadValue(String),
    /// The values together are invalid (named parameter).
    Invalid(&'static str),
}

impl fmt::Display for TunableError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Syntax(line) => write!(f, "line {line}: expected `key = value  # unit`"),
            Self::Unknown(k) => write!(f, "unknown tunable `{k}`"),
            Self::Duplicate(k) => write!(f, "tunable `{k}` given twice"),
            Self::Missing(k) => write!(f, "tunable `{k}` is missing"),
            Self::MissingUnit(k) => write!(f, "tunable `{k}` does not document its unit"),
            Self::BadValue(k) => write!(f, "tunable `{k}` is not a valid number"),
            Self::Invalid(k) => write!(f, "tunable `{k}` is out of range"),
        }
    }
}

impl std::error::Error for TunableError {}

type Setter = fn(&mut Tunables, &str) -> Option<()>;

fn positive(v: &str) -> Option<u32> {
    v.parse::<u32>().ok().filter(|x| *x > 0)
}

fn f32_of(v: &str) -> Option<f32> {
    v.parse::<f32>().ok().filter(|x| x.is_finite())
}

/// Every tunable: `(table, key, setter)`.
const KEYS: &[(&str, &str, Setter)] = &[
    ("server", "tick_rate", |t, v| {
        t.tick_rate = TickRate::new(v.parse().ok()?)?;
        Some(())
    }),
    ("server", "instance_release_grace", |t, v| {
        t.instance_release_grace = v.parse().ok().filter(|x| *x > 0)?;
        Some(())
    }),
    ("motion", "run_speed", |t, v| {
        t.motion.run_speed = f32_of(v)?;
        Some(())
    }),
    ("motion", "walk_speed", |t, v| {
        t.motion.walk_speed = f32_of(v)?;
        Some(())
    }),
    ("motion", "backward_scale", |t, v| {
        t.motion.backward_scale = f32_of(v)?;
        Some(())
    }),
    ("motion", "ground_accel", |t, v| {
        t.motion.ground_accel = f32_of(v)?;
        Some(())
    }),
    ("motion", "air_accel", |t, v| {
        t.motion.air_accel = f32_of(v)?;
        Some(())
    }),
    ("motion", "jump_speed", |t, v| {
        t.motion.jump_speed = f32_of(v)?;
        Some(())
    }),
    ("motion", "gravity", |t, v| {
        t.motion.gravity = f32_of(v)?;
        Some(())
    }),
    ("motion", "max_fall_speed", |t, v| {
        t.motion.max_fall_speed = f32_of(v)?;
        Some(())
    }),
    ("motion", "max_step_up", |t, v| {
        t.motion.max_step_up = f32_of(v)?;
        Some(())
    }),
    ("motion", "max_slope", |t, v| {
        t.motion.max_slope = f32_of(v)?;
        Some(())
    }),
    ("motion", "ground_snap", |t, v| {
        t.motion.ground_snap = f32_of(v)?;
        Some(())
    }),
    ("envelope", "speed_tolerance", |t, v| {
        t.envelope.speed_tolerance = f32_of(v).filter(|x| *x >= 0.0)?;
        Some(())
    }),
    ("envelope", "distance_slack", |t, v| {
        t.envelope.distance_slack = f32_of(v).filter(|x| *x >= 0.0)?;
        Some(())
    }),
    ("envelope", "vertical_tolerance", |t, v| {
        t.envelope.vertical_tolerance = f32_of(v).filter(|x| *x >= 0.0)?;
        Some(())
    }),
    ("envelope", "jitter_allowance_ms", |t, v| {
        t.envelope.jitter_allowance_ms = v.parse::<i64>().ok().filter(|x| *x >= 0)?;
        Some(())
    }),
    ("envelope", "resync_window_ms", |t, v| {
        t.envelope.resync_window_ms = v.parse::<i64>().ok().filter(|x| *x >= 0)?;
        Some(())
    }),
    ("interest", "near", |t, v| {
        t.interest.near = f32_of(v).filter(|x| *x > 0.0)?;
        Some(())
    }),
    ("interest", "mid", |t, v| {
        t.interest.mid = f32_of(v).filter(|x| *x > 0.0)?;
        Some(())
    }),
    ("interest", "far", |t, v| {
        t.interest.far = f32_of(v).filter(|x| *x > 0.0)?;
        Some(())
    }),
    ("interest", "hysteresis", |t, v| {
        t.interest.hysteresis = f32_of(v).filter(|x| *x >= 0.0)?;
        Some(())
    }),
    ("interest", "weight_near", |t, v| {
        t.interest.weights[0] = v.parse().ok()?;
        Some(())
    }),
    ("interest", "weight_mid", |t, v| {
        t.interest.weights[1] = v.parse().ok()?;
        Some(())
    }),
    ("interest", "weight_far", |t, v| {
        t.interest.weights[2] = v.parse().ok()?;
        Some(())
    }),
    ("interest", "budget", |t, v| {
        t.interest.budget = v.parse().ok().filter(|x| *x > 0)?;
        Some(())
    }),
    ("limits", "input_rate", |t, v| {
        t.limits.inputs.per_second = positive(v)?;
        Some(())
    }),
    ("limits", "input_burst", |t, v| {
        t.limits.inputs.burst = positive(v)?;
        Some(())
    }),
    ("limits", "action_rate", |t, v| {
        t.limits.actions.per_second = positive(v)?;
        Some(())
    }),
    ("limits", "action_burst", |t, v| {
        t.limits.actions.burst = positive(v)?;
        Some(())
    }),
    ("limits", "extension_rate", |t, v| {
        t.limits.extensions.per_second = positive(v)?;
        Some(())
    }),
    ("limits", "extension_burst", |t, v| {
        t.limits.extensions.burst = positive(v)?;
        Some(())
    }),
    ("limits", "ack_rate", |t, v| {
        t.limits.acks.per_second = positive(v)?;
        Some(())
    }),
    ("limits", "ack_burst", |t, v| {
        t.limits.acks.burst = positive(v)?;
        Some(())
    }),
    ("limits", "bytes_per_second", |t, v| {
        t.limits.bytes_per_second = positive(v)?;
        Some(())
    }),
    ("limits", "kick_after", |t, v| {
        t.limits.kick_after = positive(v)?;
        Some(())
    }),
];

impl Tunables {
    /// The keys every manifest must give, as `table.key`.
    pub fn keys() -> impl Iterator<Item = (&'static str, &'static str)> {
        KEYS.iter().map(|(table, key, _)| (*table, *key))
    }

    /// The defaults compiled into the server.
    ///
    /// # Errors
    /// [`TunableError`] if the shipped manifest is broken (a test guards it).
    pub fn defaults() -> Result<Self, TunableError> {
        Self::parse(PACKAGE_TOML)
    }

    /// Reads the `[tunables.*]` tables of a manifest.
    ///
    /// # Errors
    /// [`TunableError`].
    pub fn parse(text: &str) -> Result<Self, TunableError> {
        let mut t = Self {
            tick_rate: TickRate::HZ_30,
            motion: MotionParams::DEFAULT,
            envelope: EnvelopeConfig::DEFAULT,
            interest: TierConfig::DEFAULT,
            instance_release_grace: 30,
            limits: RateLimits::DEFAULT,
            content: ContentHash::of(text.as_bytes()),
        };
        let mut seen = vec![false; KEYS.len()];
        let mut table: Option<&str> = None;
        for (n, raw) in text.lines().enumerate() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some(header) = line.strip_prefix('[') {
                let name = header
                    .strip_suffix(']')
                    .ok_or(TunableError::Syntax(n + 1))?
                    .trim();
                table = name.strip_prefix("tunables.");
                continue;
            }
            let Some(table) = table else { continue };
            let (assignment, comment) = line.split_once('#').unwrap_or((line, ""));
            let (key, value) = assignment.split_once('=').ok_or(TunableError::Syntax(n + 1))?;
            let (key, value) = (key.trim(), value.trim());
            let full = format!("{table}.{key}");
            let index = KEYS
                .iter()
                .position(|(tb, k, _)| *tb == table && *k == key)
                .ok_or_else(|| TunableError::Unknown(full.clone()))?;
            if comment.trim().is_empty() {
                return Err(TunableError::MissingUnit(full));
            }
            match seen.get_mut(index) {
                Some(s) if *s => return Err(TunableError::Duplicate(full)),
                Some(s) => *s = true,
                None => return Err(TunableError::Unknown(full)),
            }
            let set = KEYS
                .get(index)
                .map(|(_, _, s)| *s)
                .ok_or_else(|| TunableError::Unknown(full.clone()))?;
            set(&mut t, value).ok_or(TunableError::BadValue(full))?;
        }
        if let Some(missing) = KEYS.iter().zip(&seen).find(|(_, s)| !**s) {
            return Err(TunableError::Missing(missing.0.1));
        }
        t.motion.validate().map_err(TunableError::Invalid)?;
        let i = &t.interest;
        if !(i.near < i.mid && i.mid < i.far) {
            return Err(TunableError::Invalid(
                "interest tiers must widen: near < mid < far",
            ));
        }
        Ok(t)
    }
}
