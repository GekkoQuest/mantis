//! Movement authority for both modes (plan 7.3, 7.5; decision 0011).
//!
//! **Predictive.** Each tick the server applies exactly one input per avatar,
//! in seq order. With no input for the next seq, it repeats the last input
//! under that seq and marks the seq consumed; a real input that arrives later
//! for a consumed seq is dropped. `ack` therefore counts `Motion::step` calls
//! one for one (the snapshot contract).
//!
//! **Validated.** Each claim is checked against the `Motion::step` envelope:
//! - ground: the claimed point must be on the ground model, within the
//!   snap tolerance below it and the maximum jump height above it;
//! - speed: horizontal distance over the client's own clock delta must not
//!   exceed the maximum speed under the current modifiers, plus tolerance;
//! - clock: the client clock may not run ahead of the server's by more than a
//!   jitter allowance relative to the best latency seen (so a client cannot
//!   buy distance by inflating timestamps).
//!
//! A violation corrects the client to its last accepted position with a hard
//! set-position and increments the session's cheat counter.
//!
//! **After a correction.** While a correction is pending, each claim is
//! checked by the full envelope from the corrected position, at the client
//! clock of the violating claim (the client cannot have applied the
//! correction before sending that claim). A claim that passes is the client
//! back in step and becomes the new baseline. Claims that fail are ignored,
//! because they may have been in flight before the correction arrived, until
//! `resync_window_ms` of server time has passed since the correction. Then the
//! client is corrected again and counted ([`Violation::CorrectionIgnored`]),
//! and the baseline clock restarts there.
//!
//! **Three holes this closes**, found by the toy package's `Relapse` bot,
//! which jumps again right after every correction:
//! 1. *A fixed resync radius banked distance.* Accepting any claim within a
//!    fixed reach of the corrected position let a cheater gain that reach on
//!    every correction cycle, faster than running. Checking the resync claim
//!    with the speed envelope at the violating claim's clock means a resync can
//!    never beat running at full speed.
//! 2. *Ignored claims could age into travel.* If the baseline clock never
//!    restarted, a client sitting at a teleported position would eventually be
//!    reachable "by running" and be accepted. Re-correcting after the resync
//!    window and restarting the baseline clock there means ignored time never
//!    turns into distance.
//! 3. *Fresh envelopes were free head starts.* The first claim after a join
//!    was bounded by server time since the join plus the clock jitter
//!    allowance, about 4.6 m of free travel, and a border crossing started a
//!    fresh envelope in the destination cell. The first claim is now bounded
//!    by server time since the join alone (a client cannot move before it
//!    learns where it stands), and the envelope and cheat counter travel with
//!    the avatar in the transfer.

use mantis_adapter_contract::core_types::{InputSeq, MoveInput, Vec3};
use mantis_core::kinematics::{GroundQuery, Motion, MotionModifiers};

use crate::session::{CellSession, EnvelopeState};

/// Validated-mode tolerances (per package tunables).
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct EnvelopeConfig {
    /// Allowed relative excess over the maximum speed (0.10 = 10%).
    pub speed_tolerance: f32,
    /// Allowed absolute excess distance per claim, in units.
    pub distance_slack: f32,
    /// How far the client clock may run ahead of its best observed latency.
    pub jitter_allowance_ms: i64,
    /// Vertical tolerance around the ground and jump envelope, in units.
    pub vertical_tolerance: f32,
    /// After a correction, how long (server time) a client may keep
    /// reporting positions the envelope cannot reach from the corrected one
    /// (claims already in flight) before it is corrected again and counted.
    pub resync_window_ms: i64,
}

impl EnvelopeConfig {
    /// Defaults tuned by the toy package's honest and cheating bots.
    pub const DEFAULT: Self = Self {
        speed_tolerance: 0.10,
        // Covers millisecond timestamp rounding (about 1% of a tick's travel)
        // and nothing more: a 20% speed excess is 0.047 units per 30 Hz tick
        // and must not hide inside the slack.
        distance_slack: 0.01,
        jitter_allowance_ms: 600,
        vertical_tolerance: 0.35,
        // Claims sent before the correction arrived drain within a round
        // trip plus a retransmission; the same bound as clock jitter.
        resync_window_ms: 600,
    };
}

/// One reported position with its timing.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Claim {
    /// The claimed position.
    pub position: Vec3,
    /// The client's clock when it was taken, in ms (untrusted).
    pub client_ms: u32,
    /// The server time it is evaluated at, in ms.
    pub server_ms: i64,
}

/// Why a claim was rejected.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Violation {
    /// No ground under the claimed point.
    OffMap,
    /// Below the ground or above the jump envelope.
    Vertical,
    /// Faster than the envelope allows.
    Speed,
    /// The client clock went backwards while moving.
    ClockBackwards,
    /// The client clock runs ahead of the server's.
    ClockAhead,
    /// After a correction, the client kept reporting unreachable positions
    /// for longer than the resync window.
    CorrectionIgnored,
}

/// Outcome of buffering one input.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Buffered {
    /// Stored for a future tick.
    Accepted,
    /// Its seq was already consumed (applied, synthesized, or duplicate).
    Stale,
    /// Its seq is beyond the window.
    TooFarAhead,
}

/// Buffers a Predictive input, keeping the buffer sorted by seq.
pub fn buffer_input(s: &mut CellSession, input: MoveInput) -> Buffered {
    let next = s.last_seq.map_or(input.seq, InputSeq::next);
    if let Some(last) = s.last_seq
        && !input.seq.is_newer_than(last)
    {
        return Buffered::Stale;
    }
    let ahead = input.seq.0.wrapping_sub(next.0);
    if ahead as usize >= crate::session::INPUT_WINDOW {
        return Buffered::TooFarAhead;
    }
    if s.inputs.iter().any(|i| i.seq == input.seq) {
        return Buffered::Stale;
    }
    if s.inputs.push(input).is_err() {
        return Buffered::TooFarAhead;
    }
    // Keep sorted in serial order from the reference: the next seq to apply,
    // or (before anything was applied) the oldest buffered seq.
    let reference = match s.last_seq {
        Some(last) => last.next(),
        None => oldest(&s.inputs).unwrap_or(next),
    };
    s.inputs
        .sort_unstable_by_key(|i| i.seq.0.wrapping_sub(reference.0));
    Buffered::Accepted
}

/// The buffered seq no other buffered seq is older than (serial order).
fn oldest(inputs: &[MoveInput]) -> Option<InputSeq> {
    inputs
        .iter()
        .map(|i| i.seq)
        .find(|a| inputs.iter().all(|b| b.seq == *a || b.seq.is_newer_than(*a)))
}

/// Takes the input for this tick: the buffered one with seq `last + 1`, or a
/// repeat of the last input under that seq (consumed). The first input of a
/// session defines the starting seq.
pub fn next_input(s: &mut CellSession) -> Option<MoveInput> {
    let Some(last) = s.last_seq else {
        // Nothing applied yet: start from the oldest buffered input, if any.
        let first = s.inputs.remove(0)?;
        s.last_seq = Some(first.seq);
        s.last_input = first;
        return Some(first);
    };
    let want = last.next();
    let input = if s.inputs.first().is_some_and(|i| i.seq == want) {
        s.inputs.remove(0).unwrap_or(s.last_input)
    } else {
        s.synthesized += 1;
        MoveInput {
            seq: want,
            ..s.last_input
        }
    };
    s.last_seq = Some(want);
    s.last_input = input;
    Some(input)
}

/// The highest point a mover can reach above the ground.
fn jump_height(motion: &Motion, mods: &MotionModifiers) -> f32 {
    let p = motion.params();
    let v = p.jump_speed * mods.jump_scale.max(0.0);
    let g = p.gravity * mods.gravity_scale.max(0.0);
    if g > 0.0 { v * v / (2.0 * g) } else { f32::MAX }
}

/// The claim is on the map, near the ground or within a jump of it.
fn check_vertical(
    cfg: &EnvelopeConfig,
    motion: &Motion,
    mods: &MotionModifiers,
    ground: &(impl GroundQuery + ?Sized),
    claim: Vec3,
) -> Result<(), Violation> {
    let Some(h) = ground.height_at(claim.x, claim.z) else {
        return Err(Violation::OffMap);
    };
    let p = motion.params();
    if claim.y < h - p.ground_snap - cfg.vertical_tolerance
        || claim.y > h + jump_height(motion, mods) + cfg.vertical_tolerance
    {
        return Err(Violation::Vertical);
    }
    Ok(())
}

/// Handles one claim for a session, including the correction protocol.
/// Returns `Ok(Some(position))` to accept, `Ok(None)` to ignore (awaiting a
/// correction), and `Err` for a violation (the caller corrects and counts).
///
/// # Errors
/// The [`Violation`].
pub fn handle_claim(
    env: &mut EnvelopeState,
    cfg: &EnvelopeConfig,
    motion: &Motion,
    mods: &MotionModifiers,
    ground: &(impl GroundQuery + ?Sized),
    claim_in: Claim,
) -> Result<Option<Vec3>, Violation> {
    let Claim {
        position: _,
        client_ms,
        server_ms,
    } = claim_in;
    if env.awaiting_correction {
        // The corrected position is the baseline, at the client clock of the
        // violating claim (the client cannot have applied the correction
        // before sending it). A claim the envelope reaches from there is the
        // client back in step, so it can never gain more than running at
        // full speed would. Unreachable claims are ignored (they may have
        // been in flight before the correction arrived) until the resync
        // window runs out; then the client is corrected again and counted.
        return match check_claim(env, cfg, motion, mods, ground, claim_in) {
            Ok(p) => {
                env.awaiting_correction = false;
                Ok(Some(p))
            }
            Err(_) if server_ms - env.corrected_ms > cfg.resync_window_ms => {
                // Corrected again: the baseline clock restarts here, so time
                // spent ignoring the correction never becomes travel.
                env.corrected_ms = server_ms;
                env.last_client_ms = Some(client_ms);
                Err(Violation::CorrectionIgnored)
            }
            Err(_) => Ok(None),
        };
    }
    match check_claim(env, cfg, motion, mods, ground, claim_in) {
        Ok(p) => Ok(Some(p)),
        Err(v) => {
            env.awaiting_correction = true;
            env.corrected_ms = server_ms;
            env.last_client_ms = Some(client_ms);
            Err(v)
        }
    }
}

/// Checks one claim against the envelope. On success, updates the envelope
/// and returns the accepted position.
///
/// # Errors
/// The [`Violation`]; the envelope keeps the last accepted position.
#[allow(clippy::cast_precision_loss)] // millisecond deltas are small
pub fn check_claim(
    env: &mut EnvelopeState,
    cfg: &EnvelopeConfig,
    motion: &Motion,
    mods: &MotionModifiers,
    ground: &(impl GroundQuery + ?Sized),
    claim_in: Claim,
) -> Result<Vec3, Violation> {
    let Claim {
        position: claim,
        client_ms,
        server_ms,
    } = claim_in;
    check_vertical(cfg, motion, mods, ground, claim)?;
    let offset = i64::from(client_ms) - server_ms;
    let best = env.min_offset_ms.map_or(offset, |m| m.min(offset));
    if offset - best > cfg.jitter_allowance_ms {
        env.min_offset_ms = Some(best);
        return Err(Violation::ClockAhead);
    }
    let dist = (claim - env.last_pos).horizontal().length();
    if let Some(last_ms) = env.last_client_ms {
        let dt_ms = client_ms.wrapping_sub(last_ms);
        if dt_ms == 0 || dt_ms > 0x8000_0000 {
            if dist > cfg.distance_slack {
                return Err(Violation::ClockBackwards);
            }
        } else {
            let max =
                motion.max_horizontal_speed(mods) * (dt_ms as f32 / 1000.0) * (1.0 + cfg.speed_tolerance)
                    + cfg.distance_slack;
            if dist > max {
                return Err(Violation::Speed);
            }
        }
    } else {
        // First claim: bounded by server time since the avatar was placed.
        // The client cannot have moved before it learned where it stands,
        // so this bound is never short for an honest client and gives a
        // cheater no head start.
        let elapsed_ms = (server_ms - env.joined_ms).max(0);
        let max =
            motion.max_horizontal_speed(mods) * (elapsed_ms as f32 / 1000.0) * (1.0 + cfg.speed_tolerance)
                + cfg.distance_slack;
        if dist > max {
            return Err(Violation::Speed);
        }
    }
    env.min_offset_ms = Some(best);
    env.last_pos = claim;
    env.last_client_ms = Some(client_ms);
    Ok(claim)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::ReplicationId;
    use mantis_adapter_contract::MovementMode;
    use mantis_adapter_contract::core_types::{EntityId, MoveButtons, Tick};
    use mantis_core::kinematics::{FlatGround, MotionParams};
    use mantis_core::log::SessionId;

    fn session() -> CellSession {
        CellSession::new(
            SessionId(1),
            MovementMode::Predictive,
            ReplicationId(EntityId::new(0, 0)),
            1,
            Vec3::ZERO,
            0,
        )
    }

    fn input(seq: u32) -> MoveInput {
        MoveInput {
            seq: InputSeq(seq),
            tick: Tick(u64::from(seq)),
            buttons: if seq.is_multiple_of(2) {
                MoveButtons::FORWARD
            } else {
                MoveButtons::NONE
            },
            ..MoveInput::default()
        }
    }

    #[test]
    fn inputs_apply_in_order_and_gaps_consume_seqs() {
        let mut s = session();
        assert_eq!(next_input(&mut s), None, "nothing yet");
        for seq in [10, 12, 11] {
            assert_eq!(buffer_input(&mut s, input(seq)), Buffered::Accepted);
        }
        assert_eq!(next_input(&mut s).map(|i| i.seq.0), Some(10));
        assert_eq!(next_input(&mut s).map(|i| i.seq.0), Some(11));
        assert_eq!(next_input(&mut s).map(|i| i.seq.0), Some(12));
        // Nothing for 13: repeat 12's input under seq 13 (consumed).
        let synth = next_input(&mut s).unwrap();
        assert_eq!(synth.seq, InputSeq(13));
        assert_eq!(synth.buttons, input(12).buttons);
        assert_eq!(s.synthesized, 1);
        assert_eq!(s.last_seq, Some(InputSeq(13)));
        // The real 13 arrives late: dropped.
        assert_eq!(buffer_input(&mut s, input(13)), Buffered::Stale);
        assert_eq!(buffer_input(&mut s, input(12)), Buffered::Stale);
        assert_eq!(buffer_input(&mut s, input(14 + 40)), Buffered::TooFarAhead);
        assert_eq!(buffer_input(&mut s, input(14)), Buffered::Accepted);
        assert_eq!(buffer_input(&mut s, input(14)), Buffered::Stale, "duplicate");
        assert_eq!(next_input(&mut s).map(|i| i.seq.0), Some(14));
    }

    #[test]
    fn seqs_wrap() {
        let mut s = session();
        buffer_input(&mut s, input(u32::MAX));
        assert_eq!(next_input(&mut s).map(|i| i.seq.0), Some(u32::MAX));
        assert_eq!(buffer_input(&mut s, input(0)), Buffered::Accepted);
        assert_eq!(next_input(&mut s).map(|i| i.seq.0), Some(0));
    }

    fn env() -> EnvelopeState {
        EnvelopeState::new(Vec3::ZERO, 0)
    }

    #[test]
    fn envelope_accepts_honest_and_rejects_fast_claims() {
        let motion = Motion::new(MotionParams::DEFAULT).unwrap();
        let cfg = EnvelopeConfig::DEFAULT;
        let g = FlatGround(0.0);
        let m = MotionModifiers::NONE;
        let mut e = env();
        // First claim at the spawn point.
        assert_eq!(
            check_claim(
                &mut e,
                &cfg,
                &motion,
                &m,
                &g,
                Claim {
                    position: Vec3::ZERO,
                    client_ms: 1000,
                    server_ms: 1000
                }
            ),
            Ok(Vec3::ZERO)
        );
        // 7 units/s for 100 ms = 0.7 units: fine.
        let ok = Vec3::new(0.7, 0.0, 0.0);
        assert_eq!(
            check_claim(
                &mut e,
                &cfg,
                &motion,
                &m,
                &g,
                Claim {
                    position: ok,
                    client_ms: 1100,
                    server_ms: 1100
                }
            ),
            Ok(ok)
        );
        // 20% faster than allowed: rejected, last position kept.
        let fast = Vec3::new(0.7 + 0.7 * 1.2, 0.0, 0.0);
        assert_eq!(
            check_claim(
                &mut e,
                &cfg,
                &motion,
                &m,
                &g,
                Claim {
                    position: fast,
                    client_ms: 1200,
                    server_ms: 1200
                }
            ),
            Err(Violation::Speed)
        );
        assert_eq!(e.last_pos, ok);
        // Haste modifiers widen the envelope.
        let haste = MotionModifiers {
            speed_scale: 1.3,
            ..m
        };
        assert!(
            check_claim(
                &mut e,
                &cfg,
                &motion,
                &haste,
                &g,
                Claim {
                    position: Vec3::new(0.7 + 0.9, 0.0, 0.0),
                    client_ms: 1300,
                    server_ms: 1300
                }
            )
            .is_ok()
        );
    }

    #[test]
    fn envelope_rejects_clock_and_vertical_cheats() {
        let motion = Motion::new(MotionParams::DEFAULT).unwrap();
        let cfg = EnvelopeConfig::DEFAULT;
        let g = FlatGround(0.0);
        let m = MotionModifiers::NONE;
        let mut e = env();
        check_claim(
            &mut e,
            &cfg,
            &motion,
            &m,
            &g,
            Claim {
                position: Vec3::ZERO,
                client_ms: 0,
                server_ms: 100,
            },
        )
        .unwrap();
        // Client clock jumps 2 s ahead of the server: buys distance, refused.
        assert_eq!(
            check_claim(
                &mut e,
                &cfg,
                &motion,
                &m,
                &g,
                Claim {
                    position: Vec3::new(14.0, 0.0, 0.0),
                    client_ms: 2000,
                    server_ms: 133
                }
            ),
            Err(Violation::ClockAhead)
        );
        // Clock backwards while moving.
        assert_eq!(
            check_claim(
                &mut e,
                &cfg,
                &motion,
                &m,
                &g,
                Claim {
                    position: Vec3::new(0.5, 0.0, 0.0),
                    client_ms: 0,
                    server_ms: 166
                }
            ),
            Err(Violation::ClockBackwards)
        );
        // Flying.
        assert_eq!(
            check_claim(
                &mut e,
                &cfg,
                &motion,
                &m,
                &g,
                Claim {
                    position: Vec3::new(0.1, 5.0, 0.0),
                    client_ms: 100,
                    server_ms: 200
                }
            ),
            Err(Violation::Vertical)
        );
        // Under the floor.
        assert_eq!(
            check_claim(
                &mut e,
                &cfg,
                &motion,
                &m,
                &g,
                Claim {
                    position: Vec3::new(0.1, -2.0, 0.0),
                    client_ms: 100,
                    server_ms: 200
                }
            ),
            Err(Violation::Vertical)
        );
        // A jump within the envelope is fine (v^2/2g = 1.6).
        assert!(
            check_claim(
                &mut e,
                &cfg,
                &motion,
                &m,
                &g,
                Claim {
                    position: Vec3::new(0.1, 1.5, 0.0),
                    client_ms: 133,
                    server_ms: 233
                }
            )
            .is_ok()
        );
    }

    #[test]
    fn delayed_bursts_from_an_honest_client_pass() {
        // Network delay then a burst: client timestamps stay honest, the
        // server sees them late and close together.
        let motion = Motion::new(MotionParams::DEFAULT).unwrap();
        let cfg = EnvelopeConfig::DEFAULT;
        let g = FlatGround(0.0);
        let m = MotionModifiers::NONE;
        let mut e = env();
        check_claim(
            &mut e,
            &cfg,
            &motion,
            &m,
            &g,
            Claim {
                position: Vec3::ZERO,
                client_ms: 0,
                server_ms: 50,
            },
        )
        .unwrap();
        for k in 1..=10u32 {
            let x = 7.0 * f32::from(u16::try_from(k).unwrap()) / 30.0;
            let client = k * 1000 / 30;
            // Delivered all at once, 400 ms late.
            let server = 50 + 400 + i64::from(k);
            assert!(
                check_claim(
                    &mut e,
                    &cfg,
                    &motion,
                    &m,
                    &g,
                    Claim {
                        position: Vec3::new(x, 0.0, 0.0),
                        client_ms: client,
                        server_ms: server
                    }
                )
                .is_ok(),
                "claim {k}"
            );
        }
    }
}

#[cfg(test)]
mod correction_tests {
    use super::*;
    use mantis_core::kinematics::{FlatGround, MotionParams};

    #[test]
    fn in_flight_claims_after_a_correction_are_ignored_not_counted() {
        let motion = Motion::new(MotionParams::DEFAULT).unwrap();
        let cfg = EnvelopeConfig::DEFAULT;
        let g = FlatGround(0.0);
        let m = MotionModifiers::NONE;
        let mut e = EnvelopeState::new(Vec3::ZERO, 0);
        assert_eq!(
            handle_claim(
                &mut e,
                &cfg,
                &motion,
                &m,
                &g,
                Claim {
                    position: Vec3::ZERO,
                    client_ms: 0,
                    server_ms: 0
                }
            ),
            Ok(Some(Vec3::ZERO))
        );
        // A teleport: violation.
        assert!(
            handle_claim(
                &mut e,
                &cfg,
                &motion,
                &m,
                &g,
                Claim {
                    position: Vec3::new(50.0, 0.0, 0.0),
                    client_ms: 33,
                    server_ms: 33
                }
            )
            .is_err()
        );
        // Claims sent before the client saw the correction: ignored.
        assert_eq!(
            handle_claim(
                &mut e,
                &cfg,
                &motion,
                &m,
                &g,
                Claim {
                    position: Vec3::new(50.2, 0.0, 0.0),
                    client_ms: 66,
                    server_ms: 66
                }
            ),
            Ok(None)
        );
        // The client applied the correction: accepted, normal checks resume.
        assert_eq!(
            handle_claim(
                &mut e,
                &cfg,
                &motion,
                &m,
                &g,
                Claim {
                    position: Vec3::ZERO,
                    client_ms: 200,
                    server_ms: 200
                }
            ),
            Ok(Some(Vec3::ZERO))
        );
        assert!(
            handle_claim(
                &mut e,
                &cfg,
                &motion,
                &m,
                &g,
                Claim {
                    position: Vec3::new(0.2, 0.0, 0.0),
                    client_ms: 233,
                    server_ms: 233
                }
            )
            .is_ok()
        );
    }

    #[test]
    fn the_first_claim_may_have_moved_since_spawn() {
        let motion = Motion::new(MotionParams::DEFAULT).unwrap();
        let cfg = EnvelopeConfig::DEFAULT;
        let g = FlatGround(0.0);
        let m = MotionModifiers::NONE;
        let mut e = EnvelopeState::new(Vec3::ZERO, 1000);
        // 300 ms after spawn, 2 units away: within 7 u/s over 300 ms + jitter.
        assert!(
            handle_claim(
                &mut e,
                &cfg,
                &motion,
                &m,
                &g,
                Claim {
                    position: Vec3::new(2.0, 0.0, 0.0),
                    client_ms: 300,
                    server_ms: 1300
                }
            )
            .is_ok()
        );
        let mut e = EnvelopeState::new(Vec3::ZERO, 1000);
        assert!(
            handle_claim(
                &mut e,
                &cfg,
                &motion,
                &m,
                &g,
                Claim {
                    position: Vec3::new(40.0, 0.0, 0.0),
                    client_ms: 300,
                    server_ms: 1300
                }
            )
            .is_err()
        );
    }
}
