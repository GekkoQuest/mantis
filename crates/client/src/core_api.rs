//! The client's view of `mantis-core`: the core types it uses, re-exported, plus the
//! client's motion seam.
//!
//! [`MotionStep`] is what the predictor is generic over. Production uses [`CoreMotion`],
//! which forwards to `mantis_core::kinematics::Motion::step`, the same function the server
//! runs (decisions 0011, 0013). Tests may substitute models.
//!
//! [`angle_from_turns`] and friends convert between the camera's turns and the wire
//! [`Angle16`]. Prediction always steps with the quantized value, never the camera's float.

use core::marker::PhantomData;

pub use mantis_core::ecs::EntityId;
pub use mantis_core::kinematics::{
    AimAngles, Angle16, FlatGround, GroundQuery, Heightfield, InputSeq, Motion, MotionModifiers,
    MotionParams, MotionState, MoveButtons, MoveInput,
};
pub use mantis_core::math::Vec3;
pub use mantis_core::time::{Clock, Tick, TickRate};

/// Quantizes an angle in turns (1.0 = full circle) to the nearest [`Angle16`] step. Any
/// finite input wraps; a non-finite input maps to zero (fail closed).
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // Range-checked below.
pub fn angle_from_turns(turns: f32) -> Angle16 {
    if !turns.is_finite() {
        return Angle16(0);
    }
    // In [0, 1) after wrapping; scaled and rounded it is in [0, 65536]; 65536 wraps to 0.
    let steps = (f64::from(turns.rem_euclid(1.0)) * 65_536.0).round() as u32;
    Angle16((steps & 0xFFFF) as u16)
}

/// An [`Angle16`] in turns, in [0, 1).
pub fn angle_to_turns(a: Angle16) -> f32 {
    f32::from(a.0) / 65_536.0
}

/// Signed shortest step distance from `from` to `to`, in [-32768, 32767].
pub fn angle_delta(from: Angle16, to: Angle16) -> i16 {
    to.delta(from)
}

/// Kinematic facts the client reads from a motion state for presentation and metrics.
pub trait AvatarKinematics: Copy + Send + Sync + 'static {
    /// World position.
    fn position(&self) -> Vec3;
    /// World velocity, units per second.
    fn velocity(&self) -> Vec3;
    /// Facing.
    fn yaw(&self) -> Angle16;
    /// Bit-identical comparison: the equality prediction is judged by.
    fn bits_eq(&self, other: &Self) -> bool;
}

impl AvatarKinematics for MotionState {
    fn position(&self) -> Vec3 {
        self.position
    }
    fn velocity(&self) -> Vec3 {
        self.velocity
    }
    fn yaw(&self) -> Angle16 {
        self.yaw
    }
    fn bits_eq(&self, other: &Self) -> bool {
        MotionState::bits_eq(self, other)
    }
}

/// One deterministic motion step: the seam the predictor is generic over.
///
/// Implementations must be pure (same arguments, same bits), allocation-free, and
/// identical to what the server runs.
pub trait MotionStep: Send + Sync + 'static {
    /// The integrated state.
    type State: AvatarKinematics;
    /// The ground model.
    type Ground: GroundQuery + Send + Sync + 'static;

    /// Integrates one tick.
    fn step(
        &self,
        ground: &Self::Ground,
        state: &Self::State,
        input: &MoveInput,
        mods: &MotionModifiers,
        dt: f32,
    ) -> Self::State;
}

/// The production motion model: core's `Motion` over ground model `G`.
#[derive(Clone, Copy, Debug)]
pub struct CoreMotion<G> {
    motion: Motion,
    ground: PhantomData<fn() -> G>,
}

impl<G> CoreMotion<G> {
    /// Wraps a configured core motion model.
    pub fn new(motion: Motion) -> Self {
        Self {
            motion,
            ground: PhantomData,
        }
    }

    /// The core model.
    pub fn motion(&self) -> &Motion {
        &self.motion
    }
}

impl<G: GroundQuery + Send + Sync + 'static> MotionStep for CoreMotion<G> {
    type State = MotionState;
    type Ground = G;

    fn step(
        &self,
        ground: &G,
        state: &MotionState,
        input: &MoveInput,
        mods: &MotionModifiers,
        dt: f32,
    ) -> MotionState {
        self.motion.step(ground, state, input, mods, dt)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn angle_turns_quantize_and_wrap() {
        assert_eq!(angle_from_turns(0.0), Angle16(0));
        assert_eq!(angle_from_turns(0.25), Angle16(16_384));
        assert_eq!(angle_from_turns(1.25), Angle16(16_384));
        assert_eq!(angle_from_turns(-0.25), Angle16(49_152));
        assert_eq!(angle_from_turns(0.999_999_9), Angle16(0));
        assert_eq!(angle_from_turns(f32::NAN), Angle16(0));
        assert_eq!(angle_to_turns(Angle16(16_384)), 0.25);
        assert_eq!(angle_delta(Angle16(65_000), Angle16(100)), 636);
        assert_eq!(angle_delta(Angle16(100), Angle16(65_000)), -636);
    }

    #[test]
    fn core_motion_forwards_to_core_step_bit_for_bit() -> Result<(), &'static str> {
        let core = Motion::new(MotionParams::DEFAULT)?;
        let seam: CoreMotion<FlatGround> = CoreMotion::new(core);
        let ground = FlatGround(0.0);
        let state = MotionState::at_rest(Vec3::ZERO, Angle16(0));
        let input = MoveInput {
            buttons: MoveButtons::FORWARD,
            ..MoveInput::default()
        };
        let mods = MotionModifiers::default();
        let a = seam.step(&ground, &state, &input, &mods, 1.0 / 30.0);
        let b = core.step(&ground, &state, &input, &mods, 1.0 / 30.0);
        assert!(a.bits_eq(&b));
        assert!(a.position.z > 0.0, "yaw 0 faces +z");
        Ok(())
    }
}
