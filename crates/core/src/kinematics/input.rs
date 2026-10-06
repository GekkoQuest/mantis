//! Quantized movement input: the payload of the `Move` intent.

use core::fmt;

use crate::hash::{StableHasher, StateHash};
use crate::time::Tick;

/// An angle quantized to 1/65536 of a turn. The wire and simulation
/// representation of yaw and aim, so both hosts integrate identical values.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Default)]
pub struct Angle16(pub u16);

const TURN: f32 = 65_536.0;
const RAD_PER_UNIT: f32 = core::f32::consts::TAU / TURN;
const UNIT_PER_RAD: f32 = TURN / core::f32::consts::TAU;

impl Angle16 {
    /// Radians in `[0, 2*pi)`: exactly `units * (2*pi / 65536)` in `f32`.
    #[must_use]
    pub fn to_radians(self) -> f32 {
        f32::from(self.0) * RAD_PER_UNIT
    }

    /// Quantizes radians (any finite value; wraps). Round to nearest unit;
    /// non-finite input maps to 0. Deterministic: one multiply, one exact
    /// rounding, integer wrap.
    #[must_use]
    #[allow(clippy::cast_possible_truncation)] // wrapped into u16 range first
    pub fn from_radians(r: f32) -> Self {
        if !r.is_finite() {
            return Self(0);
        }
        let units = (r * UNIT_PER_RAD).round();
        // |units| can exceed i64 only for |r| > 1e14; clamp, then wrap.
        let clamped = units.clamp(-9.0e15, 9.0e15) as i64;
        Self(clamped.rem_euclid(65_536) as u16)
    }

    /// Signed difference `self - other` in units, in `[-32768, 32767]`.
    #[must_use]
    pub const fn delta(self, other: Self) -> i16 {
        self.0.wrapping_sub(other.0).cast_signed()
    }
}

impl StateHash for Angle16 {
    fn state_hash(&self, h: &mut StableHasher) {
        h.write_u16(self.0);
    }
}

/// Aim direction: yaw and pitch.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub struct AimAngles {
    /// Horizontal aim.
    pub yaw: Angle16,
    /// Vertical aim (0 level; wraps like yaw).
    pub pitch: Angle16,
}

/// Input sequence number with wrapping comparison (serial-number arithmetic,
/// RFC 1982 style).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub struct InputSeq(pub u32);

impl InputSeq {
    /// The next sequence number (wraps).
    #[must_use]
    pub const fn next(self) -> Self {
        Self(self.0.wrapping_add(1))
    }

    /// True when `self` is after `other` in serial order: the forward
    /// distance from `other` to `self` is in `1..2^31`. A distance of exactly
    /// 2^31 is ambiguous and reported as not newer either way.
    #[must_use]
    pub const fn is_newer_than(self, other: Self) -> bool {
        let d = self.0.wrapping_sub(other.0);
        d != 0 && d < 0x8000_0000
    }
}

/// Movement buttons as a bit set. Unknown bits are refused at decode
/// ([`MoveButtons::from_bits`]).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct MoveButtons(u16);

impl MoveButtons {
    /// Move forward.
    pub const FORWARD: Self = Self(1 << 0);
    /// Move backward.
    pub const BACKWARD: Self = Self(1 << 1);
    /// Strafe left.
    pub const STRAFE_LEFT: Self = Self(1 << 2);
    /// Strafe right.
    pub const STRAFE_RIGHT: Self = Self(1 << 3);
    /// Jump.
    pub const JUMP: Self = Self(1 << 4);
    /// Walk instead of run.
    pub const WALK: Self = Self(1 << 5);
    /// No buttons.
    pub const NONE: Self = Self(0);
    /// Every defined button.
    pub const ALL: Self = Self(0b11_1111);

    /// The raw bits.
    #[must_use]
    pub const fn bits(self) -> u16 {
        self.0
    }

    /// Buttons from raw bits, or `None` if an undefined bit is set (fail
    /// closed at the boundary).
    #[must_use]
    pub const fn from_bits(bits: u16) -> Option<Self> {
        if bits & !Self::ALL.0 == 0 {
            Some(Self(bits))
        } else {
            None
        }
    }

    /// True when every button in `other` is held.
    #[must_use]
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    /// The union.
    #[must_use]
    pub const fn with(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    /// The difference.
    #[must_use]
    pub const fn without(self, other: Self) -> Self {
        Self(self.0 & !other.0)
    }
}

impl fmt::Debug for MoveButtons {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "MoveButtons({:#08b})", self.0)
    }
}

/// One tick of movement input: the payload of the `Move` intent.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub struct MoveInput {
    /// Client input sequence number.
    pub seq: InputSeq,
    /// The tick the input is for.
    pub tick: Tick,
    /// Held buttons.
    pub buttons: MoveButtons,
    /// Facing (sampled from the camera on the render thread, decision 0009).
    pub yaw: Angle16,
    /// Aim.
    pub aim: AimAngles,
}

impl StateHash for MoveInput {
    fn state_hash(&self, h: &mut StableHasher) {
        h.write_u32(self.seq.0);
        self.tick.state_hash(h);
        h.write_u16(self.buttons.0);
        h.write_u16(self.yaw.0);
        h.write_u16(self.aim.yaw.0);
        h.write_u16(self.aim.pitch.0);
    }
}
