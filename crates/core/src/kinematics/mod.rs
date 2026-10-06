//! Shared kinematics (plan 6.5, decisions 0011 and 0013).
//!
//! [`Motion::step`] is the one movement function. The native client runs it
//! for prediction, the server runs it to integrate `Move` inputs, and the
//! server's Validated-mode envelope is derived from the same parameters. It
//! is pure: no allocation, no interior state, no randomness, no wall clock.
//! Its result is a function of its arguments' bits only and is identical on
//! every target. Tests pin exact output bits.
//!
//! Wire-facing inputs are quantized ([`Angle16`], [`MoveButtons`]), so client
//! and server integrate literally the same values: an unquantized `f32` yaw
//! could never be reproduced by the server.
//!
//! Conventions: `y` is up; yaw 0 faces `+z` and increases toward `+x`.

mod ground;
mod input;

pub use ground::{FlatGround, GroundQuery, Heightfield, HeightfieldError};
pub use input::{AimAngles, Angle16, InputSeq, MoveButtons, MoveInput};

use crate::hash::{StableHasher, StateHash};
use crate::math::{self, Vec3};

/// The kinematic state of one mover.
#[derive(Clone, Copy, PartialEq, Debug, Default)]
pub struct MotionState {
    /// Position of the mover's feet.
    pub position: Vec3,
    /// Velocity in units per second.
    pub velocity: Vec3,
    /// Facing.
    pub yaw: Angle16,
    /// True when standing on the ground.
    pub grounded: bool,
}

impl MotionState {
    /// A grounded mover at rest.
    #[must_use]
    pub const fn at_rest(position: Vec3, yaw: Angle16) -> Self {
        Self {
            position,
            velocity: Vec3::ZERO,
            yaw,
            grounded: true,
        }
    }

    /// Bitwise equality, the comparison for prediction and replay.
    #[must_use]
    pub fn bits_eq(&self, o: &Self) -> bool {
        self.position.bits_eq(o.position)
            && self.velocity.bits_eq(o.velocity)
            && self.yaw == o.yaw
            && self.grounded == o.grounded
    }
}

impl StateHash for MotionState {
    fn state_hash(&self, h: &mut StableHasher) {
        self.position.state_hash(h);
        self.velocity.state_hash(h);
        h.write_u16(self.yaw.0);
        self.grounded.state_hash(h);
    }
}

/// Multipliers from effects (slows, hastes, ...). The server uses the same
/// values for the Validated-mode envelope.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct MotionModifiers {
    /// Scales ground and air target speed.
    pub speed_scale: f32,
    /// Scales jump launch speed.
    pub jump_scale: f32,
    /// Scales gravity.
    pub gravity_scale: f32,
}

impl Default for MotionModifiers {
    fn default() -> Self {
        Self::NONE
    }
}

impl MotionModifiers {
    /// No modification.
    pub const NONE: Self = Self {
        speed_scale: 1.0,
        jump_scale: 1.0,
        gravity_scale: 1.0,
    };
}

/// Movement tuning, loaded from package content.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct MotionParams {
    /// Ground speed when running, units per second.
    pub run_speed: f32,
    /// Ground speed when the walk button is held.
    pub walk_speed: f32,
    /// Multiplier applied when moving backward without forward input.
    pub backward_scale: f32,
    /// Horizontal acceleration on the ground, units per second squared.
    pub ground_accel: f32,
    /// Horizontal acceleration in the air.
    pub air_accel: f32,
    /// Vertical launch speed of a jump.
    pub jump_speed: f32,
    /// Gravity, units per second squared (positive).
    pub gravity: f32,
    /// Largest fall speed.
    pub max_fall_speed: f32,
    /// Highest rise a grounded mover climbs in one tick.
    pub max_step_up: f32,
    /// Steepest walkable ground, as rise over horizontal run (1.0 is 45
    /// degrees). Steeper ground blocks like a wall, whatever the step size,
    /// so a mover cannot creep up a cliff in small steps.
    pub max_slope: f32,
    /// Largest drop a grounded mover follows without becoming airborne.
    pub ground_snap: f32,
}

impl MotionParams {
    /// Generic defaults for a humanoid mover, for tests and the toy package.
    pub const DEFAULT: Self = Self {
        run_speed: 7.0,
        walk_speed: 2.5,
        backward_scale: 0.6,
        ground_accel: 60.0,
        air_accel: 8.0,
        jump_speed: 8.0,
        gravity: 20.0,
        max_fall_speed: 50.0,
        max_step_up: 0.45,
        max_slope: 1.0,
        ground_snap: 0.3,
    };

    /// Checks that every parameter is finite and in range.
    ///
    /// # Errors
    /// The name of the first invalid parameter.
    pub fn validate(&self) -> Result<(), &'static str> {
        let checks: [(&'static str, f32, bool); 11] = [
            ("run_speed", self.run_speed, true),
            ("walk_speed", self.walk_speed, true),
            ("backward_scale", self.backward_scale, true),
            ("ground_accel", self.ground_accel, false),
            ("air_accel", self.air_accel, true),
            ("jump_speed", self.jump_speed, true),
            ("gravity", self.gravity, true),
            ("max_fall_speed", self.max_fall_speed, false),
            ("max_step_up", self.max_step_up, true),
            ("max_slope", self.max_slope, true),
            ("ground_snap", self.ground_snap, true),
        ];
        for (name, v, zero_ok) in checks {
            if !v.is_finite() || v < 0.0 || (!zero_ok && v == 0.0) {
                return Err(name);
            }
        }
        Ok(())
    }
}

/// The movement integrator. Holds only immutable tuning.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Motion {
    params: MotionParams,
}

impl Motion {
    /// An integrator with validated `params`.
    ///
    /// # Errors
    /// The name of the first invalid parameter.
    pub fn new(params: MotionParams) -> Result<Self, &'static str> {
        params.validate()?;
        Ok(Self { params })
    }

    /// The tuning.
    #[must_use]
    pub const fn params(&self) -> &MotionParams {
        &self.params
    }

    /// The fastest horizontal speed `step` can produce under `mods`: the
    /// Validated-mode envelope's speed bound (decision 0011).
    #[must_use]
    pub fn max_horizontal_speed(&self, mods: &MotionModifiers) -> f32 {
        self.params.run_speed.max(self.params.walk_speed) * mods.speed_scale.max(0.0)
    }

    /// The horizontal velocity the input asks for.
    fn wish_velocity(&self, input: &MoveInput, mods: &MotionModifiers) -> Vec3 {
        let b = input.buttons;
        let forward = f32::from(
            i8::from(b.contains(MoveButtons::FORWARD)) - i8::from(b.contains(MoveButtons::BACKWARD)),
        );
        let strafe = f32::from(
            i8::from(b.contains(MoveButtons::STRAFE_RIGHT)) - i8::from(b.contains(MoveButtons::STRAFE_LEFT)),
        );
        if forward == 0.0 && strafe == 0.0 {
            return Vec3::ZERO;
        }
        let (s, c) = math::sin_cos(input.yaw.to_radians());
        let fwd = Vec3::new(s, 0.0, c);
        let right = Vec3::new(c, 0.0, -s);
        let dir = (fwd * forward + right * strafe).normalize_or_zero();
        let base = if b.contains(MoveButtons::WALK) {
            self.params.walk_speed
        } else {
            self.params.run_speed
        };
        let back = if forward < 0.0 {
            self.params.backward_scale
        } else {
            1.0
        };
        dir * (base * back * mods.speed_scale.max(0.0))
    }

    /// Advances `state` by `dt` seconds under `input`. Pure and
    /// deterministic; see the module docs.
    ///
    /// Order of operations: facing; horizontal acceleration toward the wished
    /// velocity (ground or air rate); jump; gravity; integration; then the
    /// ground rules (step up, snap down, land, or become airborne). A move
    /// that would leave the ground model, climb more than `max_step_up`, or
    /// climb steeper than `max_slope` keeps its vertical motion but cancels
    /// the horizontal one (fail closed).
    #[must_use]
    pub fn step(
        &self,
        ground: &(impl GroundQuery + ?Sized),
        state: &MotionState,
        input: &MoveInput,
        mods: &MotionModifiers,
        dt: f32,
    ) -> MotionState {
        let p = &self.params;
        let mut next = *state;
        next.yaw = input.yaw;
        if !(dt > 0.0 && dt.is_finite()) {
            return next;
        }

        // Horizontal velocity: approach the wish at a bounded rate.
        let wish = self.wish_velocity(input, mods);
        let accel = if state.grounded {
            p.ground_accel
        } else {
            p.air_accel
        };
        let current = state.velocity.horizontal();
        let delta = wish - current;
        let max_delta = accel * dt;
        let dist = delta.length();
        let horizontal = if dist <= max_delta || dist == 0.0 {
            wish
        } else {
            current + delta * (max_delta / dist)
        };
        let mut vy = state.velocity.y;
        let mut grounded = state.grounded;

        if grounded && input.buttons.contains(MoveButtons::JUMP) {
            vy = p.jump_speed * mods.jump_scale.max(0.0);
            grounded = false;
        }
        if !grounded {
            vy = (vy - p.gravity * mods.gravity_scale.max(0.0) * dt).max(-p.max_fall_speed);
        }

        let start = state.position;
        let mut pos = Vec3::new(
            start.x + horizontal.x * dt,
            start.y + vy * dt,
            start.z + horizontal.z * dt,
        );
        let mut vel = Vec3::new(horizontal.x, vy, horizontal.z);

        let mut ground_h = ground.height_at(pos.x, pos.z);
        let blocked = match ground_h {
            None => true,
            Some(h) => {
                let rise = h - start.y;
                let run = (pos - start).horizontal().length();
                state.grounded && grounded && rise > 0.0 && (rise > p.max_step_up || rise > p.max_slope * run)
            }
        };
        if blocked {
            pos.x = start.x;
            pos.z = start.z;
            vel.x = 0.0;
            vel.z = 0.0;
            ground_h = ground.height_at(pos.x, pos.z);
        }

        match ground_h {
            Some(h) if grounded => {
                if pos.y - h <= p.ground_snap {
                    pos.y = h; // step up or snap down
                    vel.y = 0.0;
                } else {
                    grounded = false; // walked off a ledge
                }
            }
            Some(h) if pos.y <= h && vel.y <= 0.0 => {
                pos.y = h; // landed
                vel.y = 0.0;
                grounded = true;
            }
            Some(_) | None => {}
        }

        next.position = pos;
        next.velocity = vel;
        next.grounded = grounded;
        next
    }
}

#[cfg(test)]
mod tests;
