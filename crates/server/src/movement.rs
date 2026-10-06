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

/// How a cell consumes Predictive inputs when they arrive late.
///
/// The cell applies one input per tick, in seq order; a seq whose input
/// has not arrived is synthesized as a repeat. After a sustained rise in
/// latency every input would then arrive just after its seq was consumed,
/// forever. So a real input arriving for a seq already synthesized earns a
/// *credit*, and a tick whose input is missing spends one to *pause*:
/// nothing is consumed, the avatar holds, and the buffer gains a tick of
/// lead. The lead adapts to the lateness observed, like a jitter buffer.
///
/// Bounds: at most one pause per `pause_every` ticks, credits at most
/// `max_lead`, and no pause while `max_lead` inputs are already buffered.
/// A client gains nothing by delaying or withholding inputs: consumption is
/// never faster than one input per tick, so a pause only costs its avatar
/// that tick, and withheld inputs are synthesized and corrected as before.
/// Every decision derives from session state fed by logged intents, so a
/// replay makes the same ones.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct InputConfig {
    /// Most ticks of lead realignment may build (credits, and buffered
    /// inputs above which no pause is taken).
    pub max_lead: u8,
    /// Ticks between pauses, at least.
    pub pause_every: u32,
    /// Seqs ahead of the next one to apply that are buffered; an input
    /// further ahead is dropped. Covers a client at the worst supported
    /// round trip plus the most lead realignment builds
    /// ([`InputConfig::for_rate`]).
    pub window: usize,
}

impl InputConfig {
    /// 400 ms round trips and 200 ms of lead at 30 Hz, a pause at most
    /// every 4 ticks.
    pub const DEFAULT: Self = Self {
        max_lead: 6,
        pause_every: 4,
        window: 20,
    };

    /// The configuration for a package at `hz`: a lead of `max_lead_ms`
    /// (rounded up, at least a tick), pauses at most every `pause_every`
    /// ticks, and a window for round trips up to `max_rtt_ms`: the round
    /// trip in ticks (a client runs ahead of the cell by about its round
    /// trip at worst) plus the lead plus 2, at most
    /// [`crate::session::INPUT_WINDOW_MAX`].
    #[must_use]
    pub fn for_rate(max_rtt_ms: u32, max_lead_ms: u32, hz: u32, pause_every: u32) -> Self {
        let ticks = |ms: u32| (u64::from(ms) * u64::from(hz)).div_ceil(1000);
        let lead = ticks(max_lead_ms).max(1);
        let window = ticks(max_rtt_ms) + lead + 2;
        Self {
            max_lead: u8::try_from(lead).unwrap_or(u8::MAX),
            pause_every: pause_every.max(1),
            window: usize::try_from(window)
                .unwrap_or(usize::MAX)
                .min(crate::session::INPUT_WINDOW_MAX),
        }
    }
}

impl Default for InputConfig {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// A Predictive session's input state that travels with its avatar across
/// a border, so a crossing never loses the lead the session built or the
/// input it repeats. (Its buffered inputs travel as `Move` intents logged by
/// the destination.)
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct InputCarry {
    /// The last applied input.
    pub last_input: MoveInput,
    /// Seqs synthesized so far.
    pub synthesized: u32,
    /// Which recent seqs were synthesized.
    pub synth_mask: u64,
    /// Realigning pauses owed.
    pub credits: u8,
    /// Ticks until the next pause.
    pub cooldown: u32,
    /// Late inputs so far.
    pub late: u32,
    /// Pauses so far.
    pub pauses: u32,
    /// Seqs skipped so far.
    pub skipped: u32,
}

impl InputCarry {
    /// The state of `s`.
    #[must_use]
    pub fn of(s: &CellSession) -> Self {
        Self {
            last_input: s.last_input,
            synthesized: s.synthesized,
            synth_mask: s.synth_mask,
            credits: s.credits,
            cooldown: s.cooldown,
            late: s.late,
            pauses: s.pauses,
            skipped: s.skipped,
        }
    }

    /// Restores it into `s`.
    pub fn apply(&self, s: &mut CellSession) {
        s.last_input = self.last_input;
        s.synthesized = self.synthesized;
        s.synth_mask = self.synth_mask;
        s.credits = self.credits;
        s.cooldown = self.cooldown;
        s.late = self.late;
        s.pauses = self.pauses;
        s.skipped = self.skipped;
    }

    /// Writes it (logs and snapshots).
    pub fn encode(&self, e: &mut mantis_core::wire::Encoder<'_>) {
        mantis_core::wire::Wire::encode(&self.last_input, e);
        e.u32(self.synthesized);
        e.u64(self.synth_mask);
        e.u8(self.credits);
        e.u32(self.cooldown);
        e.u32(self.late);
        e.u32(self.pauses);
        e.u32(self.skipped);
    }

    /// Reads what [`InputCarry::encode`] wrote.
    ///
    /// # Errors
    /// A malformed record.
    pub fn decode(d: &mut mantis_core::wire::Decoder<'_>) -> Result<Self, mantis_core::wire::DecodeError> {
        Ok(Self {
            last_input: <MoveInput as mantis_core::wire::Wire>::decode(d)?,
            synthesized: d.u32()?,
            synth_mask: d.u64()?,
            credits: d.u8()?,
            cooldown: d.u32()?,
            late: d.u32()?,
            pauses: d.u32()?,
            skipped: d.u32()?,
        })
    }
}

/// Outcome of buffering one input.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Buffered {
    /// Stored for a future tick.
    Accepted,
    /// Its seq was already consumed (applied, or a duplicate).
    Stale,
    /// Its seq was synthesized before it arrived: it is dropped, and counts
    /// toward realigning ([`InputConfig`]).
    Late,
    /// Its seq was beyond the window: the seqs before the window's new start
    /// were skipped unapplied, and it was stored.
    Skipped,
}

/// Buffers a Predictive input, keeping the buffer sorted by seq.
pub fn buffer_input(s: &mut CellSession, input: MoveInput, cfg: InputConfig) -> Buffered {
    let next = s.last_seq.map_or(input.seq, InputSeq::next);
    if let Some(last) = s.last_seq
        && !input.seq.is_newer_than(last)
    {
        let age = last.0.wrapping_sub(input.seq.0);
        let bit = 1u64.checked_shl(age).unwrap_or(0);
        if s.synth_mask & bit != 0 {
            // Counted once: a resent copy is a duplicate.
            s.synth_mask &= !bit;
            s.late = s.late.saturating_add(1);
            s.credits = s.credits.saturating_add(1);
            return Buffered::Late;
        }
        return Buffered::Stale;
    }
    let ahead = input.seq.0.wrapping_sub(next.0);
    let mut outcome = Buffered::Accepted;
    if ahead as usize >= cfg.window
        && let Some(last) = s.last_seq
    {
        // The client runs further ahead than the window: the cell's clock
        // slipped (a host stall re-anchors, never bursts), or the client
        // jumped. Skip seqs so this input leads by `max_lead`: they are
        // consumed without being applied (the avatar holds for them, so a
        // client gains nothing by running ahead), and the predictions that
        // assumed them are corrected once. Inputs before the new start go.
        let lead = u32::from(cfg.max_lead.max(1));
        let new_last = InputSeq(input.seq.0.wrapping_sub(lead));
        s.skipped = s.skipped.saturating_add(new_last.0.wrapping_sub(last.0));
        s.last_seq = Some(new_last);
        s.synth_mask = 0;
        s.inputs.retain(|i| i.seq.is_newer_than(new_last));
        outcome = Buffered::Skipped;
    }
    if s.inputs.iter().any(|i| i.seq == input.seq) {
        return Buffered::Stale;
    }
    if s.inputs.push(input).is_err() {
        return Buffered::Stale;
    }
    // Keep sorted in serial order from the reference: the next seq to apply,
    // or (before anything was applied) the oldest buffered seq.
    let reference = match s.last_seq {
        Some(last) => last.next(),
        None => oldest(&s.inputs).unwrap_or(next),
    };
    s.inputs
        .sort_unstable_by_key(|i| i.seq.0.wrapping_sub(reference.0));
    outcome
}

/// The buffered seq no other buffered seq is older than (serial order).
fn oldest(inputs: &[MoveInput]) -> Option<InputSeq> {
    inputs
        .iter()
        .map(|i| i.seq)
        .find(|a| inputs.iter().all(|b| b.seq == *a || b.seq.is_newer_than(*a)))
}

/// Takes the input for this tick: the buffered one with seq `last + 1`, or
/// a repeat of the last input under that seq (consumed), or nothing when
/// the session pauses to realign ([`InputConfig`]). The first input of a
/// session defines the starting seq.
pub fn next_input(s: &mut CellSession, cfg: InputConfig) -> Option<MoveInput> {
    s.cooldown = s.cooldown.saturating_sub(1);
    s.credits = s.credits.min(cfg.max_lead);
    let Some(last) = s.last_seq else {
        // Nothing applied yet: start from the oldest buffered input, if any.
        let first = s.inputs.remove(0)?;
        s.last_seq = Some(first.seq);
        s.last_input = first;
        return Some(first);
    };
    let want = last.next();
    let input = if s.inputs.first().is_some_and(|i| i.seq == want) {
        s.synth_mask <<= 1;
        s.inputs.remove(0).unwrap_or(s.last_input)
    } else {
        if s.credits > 0 && s.cooldown == 0 && s.inputs.len() < usize::from(cfg.max_lead) {
            s.credits -= 1;
            s.cooldown = cfg.pause_every;
            s.pauses = s.pauses.saturating_add(1);
            return None;
        }
        s.synthesized += 1;
        s.synth_mask = (s.synth_mask << 1) | 1;
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
#[expect(clippy::cast_precision_loss)] // millisecond deltas are small
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

    fn buffer_input_d(s: &mut CellSession, input: MoveInput) -> Buffered {
        buffer_input(s, input, InputConfig::DEFAULT)
    }

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
        assert_eq!(next_input(&mut s, InputConfig::DEFAULT), None, "nothing yet");
        for seq in [10, 12, 11] {
            assert_eq!(buffer_input_d(&mut s, input(seq)), Buffered::Accepted);
        }
        assert_eq!(
            next_input(&mut s, InputConfig::DEFAULT).map(|i| i.seq.0),
            Some(10)
        );
        assert_eq!(
            next_input(&mut s, InputConfig::DEFAULT).map(|i| i.seq.0),
            Some(11)
        );
        assert_eq!(
            next_input(&mut s, InputConfig::DEFAULT).map(|i| i.seq.0),
            Some(12)
        );
        // Nothing for 13: repeat 12's input under seq 13 (consumed).
        let synth = next_input(&mut s, InputConfig::DEFAULT).unwrap();
        assert_eq!(synth.seq, InputSeq(13));
        assert_eq!(synth.buttons, input(12).buttons);
        assert_eq!(s.synthesized, 1);
        assert_eq!(s.last_seq, Some(InputSeq(13)));
        // The real 13 arrives late: dropped, and counted toward realigning
        // (once: a resent copy is a duplicate).
        assert_eq!(buffer_input_d(&mut s, input(13)), Buffered::Late);
        assert_eq!(buffer_input_d(&mut s, input(13)), Buffered::Stale);
        assert_eq!((s.late, s.credits), (1, 1));
        assert_eq!(buffer_input_d(&mut s, input(12)), Buffered::Stale);
        assert_eq!(buffer_input_d(&mut s, input(14)), Buffered::Accepted);
        assert_eq!(buffer_input_d(&mut s, input(14)), Buffered::Stale, "duplicate");
        assert_eq!(
            next_input(&mut s, InputConfig::DEFAULT).map(|i| i.seq.0),
            Some(14)
        );
        // 15 is missing: a credit pauses consumption for a tick (nothing
        // applied), then not again before `pause_every` ticks.
        assert_eq!(next_input(&mut s, InputConfig::DEFAULT), None);
        assert_eq!((s.pauses, s.credits, s.last_seq), (1, 0, Some(InputSeq(14))));
        // An input beyond the window: the seqs before it are skipped
        // unapplied so it leads by `max_lead`.
        let lead = u32::from(InputConfig::DEFAULT.max_lead);
        assert_eq!(buffer_input_d(&mut s, input(15 + 40)), Buffered::Skipped);
        assert_eq!(s.last_seq, Some(InputSeq(55 - lead)));
        assert_eq!(s.skipped, 55 - lead - 14);
        assert_eq!(s.inputs.len(), 1);
    }

    #[test]
    fn seqs_wrap() {
        let mut s = session();
        buffer_input_d(&mut s, input(u32::MAX));
        assert_eq!(
            next_input(&mut s, InputConfig::DEFAULT).map(|i| i.seq.0),
            Some(u32::MAX)
        );
        assert_eq!(buffer_input_d(&mut s, input(0)), Buffered::Accepted);
        assert_eq!(next_input(&mut s, InputConfig::DEFAULT).map(|i| i.seq.0), Some(0));
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
