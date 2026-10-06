//! Cell snapshots (decision 0007): the whole simulation state of a cell at
//! a tick boundary, restored into a cell built the same way (the same
//! configuration, adapters, and module set), so its state hash equals the
//! saved one and the log after that tick replays on top of it.
//!
//! A snapshot names the build that wrote it and is read by any build of the
//! same format version: after a deploy, recovery is snapshot-only, and the
//! old build's log segments are refused by their header (decision 0007).
//!
//! What is saved: every entity and component, every resource that holds
//! state (resources that installation rebuilds exactly answer
//! [`Saved::Rebuilt`](mantis_core::ecs::Saved::Rebuilt)), and the cell's own state outside the world: the
//! tick, the rewind buffer (lag compensation decides refusals), the ghosts
//! last received, the transfers offered and not yet acknowledged, and the
//! script sources reloads named. Clients are not saved: every session comes
//! back without a connection, and the host ends them
//! ([`crate::cell::Cell::detached_sessions`]).

use mantis_adapter_contract::core_types::{ContentHash, DecodeError, Decoder, Encoder, Vec3, Wire};
use mantis_adapter_contract::{AppearanceId, MovementMode};
use mantis_core::kinematics::{Angle16, InputSeq, MotionModifiers, MotionState, MoveInput};
use mantis_core::log::{BuildId, CellId, SessionId};
use mantis_core::time::Tick;

use crate::components::ReplicationId;
use crate::interest::Replicated;
use crate::session::{CellSession, EnvelopeState};

/// A snapshot's first bytes.
pub const MAGIC: [u8; 4] = *b"MSNP";
/// The snapshot format version (bumped on any layout change).
pub const VERSION: u16 = 1;

/// What a snapshot says about itself.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SnapshotHeader {
    /// The build that wrote it.
    pub build: BuildId,
    /// The content it ran with.
    pub content: ContentHash,
    /// The cell.
    pub cell: CellId,
    /// The last tick it contains (the next simulated is `tick + 1`).
    pub tick: Tick,
    /// The world state hash at that tick.
    pub state_hash: u64,
}

impl SnapshotHeader {
    /// Reads the header of a snapshot.
    ///
    /// # Errors
    /// Not a snapshot, or another format version.
    pub fn read(d: &mut Decoder<'_>) -> Result<Self, DecodeError> {
        if d.take(4)? != MAGIC {
            return Err(DecodeError::Invalid("not a cell snapshot"));
        }
        if d.u16()? != VERSION {
            return Err(DecodeError::Invalid("snapshot format version"));
        }
        let mut build = [0u8; 32];
        build.copy_from_slice(d.take(32)?);
        Ok(Self {
            build: BuildId(build),
            content: ContentHash::decode(d)?,
            cell: CellId(d.u64()?),
            tick: Tick(d.u64()?),
            state_hash: d.u64()?,
        })
    }

    /// Writes the header.
    pub fn write(&self, e: &mut Encoder<'_>) {
        e.bytes(&MAGIC);
        e.u16(VERSION);
        e.bytes(&self.build.0);
        self.content.encode(e);
        e.u64(self.cell.0);
        e.u64(self.tick.0);
        e.u64(self.state_hash);
    }
}

// ---- exact encodings (floats as raw bits) ---------------------------------

pub(crate) fn f32_bits(e: &mut Encoder<'_>, v: f32) {
    e.u32(v.to_bits());
}

pub(crate) fn f32_of(d: &mut Decoder<'_>) -> Result<f32, DecodeError> {
    Ok(f32::from_bits(d.u32()?))
}

pub(crate) fn vec3(e: &mut Encoder<'_>, v: Vec3) {
    f32_bits(e, v.x);
    f32_bits(e, v.y);
    f32_bits(e, v.z);
}

pub(crate) fn vec3_of(d: &mut Decoder<'_>) -> Result<Vec3, DecodeError> {
    Ok(Vec3::new(f32_of(d)?, f32_of(d)?, f32_of(d)?))
}

pub(crate) fn motion(e: &mut Encoder<'_>, m: &MotionState) {
    vec3(e, m.position);
    vec3(e, m.velocity);
    e.u16(m.yaw.0);
    e.bool(m.grounded);
}

pub(crate) fn motion_of(d: &mut Decoder<'_>) -> Result<MotionState, DecodeError> {
    Ok(MotionState {
        position: vec3_of(d)?,
        velocity: vec3_of(d)?,
        yaw: Angle16(d.u16()?),
        grounded: d.bool()?,
    })
}

pub(crate) fn mods(e: &mut Encoder<'_>, m: &MotionModifiers) {
    f32_bits(e, m.speed_scale);
    f32_bits(e, m.jump_scale);
    f32_bits(e, m.gravity_scale);
}

pub(crate) fn mods_of(d: &mut Decoder<'_>) -> Result<MotionModifiers, DecodeError> {
    Ok(MotionModifiers {
        speed_scale: f32_of(d)?,
        jump_scale: f32_of(d)?,
        gravity_scale: f32_of(d)?,
    })
}

fn envelope(e: &mut Encoder<'_>, v: &EnvelopeState) {
    vec3(e, v.last_pos);
    crate::intent::put_opt_u32(e, v.last_client_ms);
    crate::intent::put_opt_i64(e, v.min_offset_ms);
    e.i64(v.joined_ms);
    e.bool(v.awaiting_correction);
    e.i64(v.corrected_ms);
}

fn envelope_of(d: &mut Decoder<'_>) -> Result<EnvelopeState, DecodeError> {
    Ok(EnvelopeState {
        last_pos: vec3_of(d)?,
        last_client_ms: crate::intent::get_opt_u32(d)?,
        min_offset_ms: crate::intent::get_opt_i64(d)?,
        joined_ms: d.i64()?,
        awaiting_correction: d.bool()?,
        corrected_ms: d.i64()?,
    })
}

/// One session, exactly.
pub(crate) fn session(e: &mut Encoder<'_>, s: &CellSession) {
    e.u64(s.id.0);
    s.mode.encode(e);
    e.bool(s.avatar.is_some());
    e.u64(s.avatar.map_or(0, mantis_core::ecs::EntityId::to_bits));
    e.u64(s.repl.0.to_bits());
    e.u64(s.epoch);
    e.u64(s.character);
    e.u32(u32::try_from(s.inputs.len()).unwrap_or(u32::MAX));
    for i in s.inputs.iter() {
        i.encode(e);
    }
    e.bool(s.last_seq.is_some());
    e.u32(s.last_seq.map_or(0, |q| q.0));
    s.last_input.encode(e);
    e.u32(u32::try_from(s.claims.len()).unwrap_or(u32::MAX));
    for (p, ms) in s.claims.iter() {
        vec3(e, *p);
        e.u32(*ms);
    }
    envelope(e, &s.envelope);
    e.u32(s.cheats);
    e.u32(s.synthesized);
    e.u64(s.synth_mask);
    e.u8(s.credits);
    e.u32(s.cooldown);
    e.u32(s.late);
    e.u32(s.pauses);
    e.u32(s.skipped);
}

/// Reads one session written by [`session`].
pub(crate) fn session_of(d: &mut Decoder<'_>) -> Result<CellSession, DecodeError> {
    let id = SessionId(d.u64()?);
    let mode = MovementMode::decode(d)?;
    let has_avatar = d.bool()?;
    let avatar = mantis_core::ecs::EntityId::from_bits(d.u64()?);
    let repl = ReplicationId(mantis_core::ecs::EntityId::from_bits(d.u64()?));
    let epoch = d.u64()?;
    let mut s = CellSession::new(id, mode, repl, epoch, Vec3::ZERO, 0);
    s.avatar = has_avatar.then_some(avatar);
    s.character = d.u64()?;
    let n = d.u32()?;
    for _ in 0..n {
        s.inputs
            .push(MoveInput::decode(d)?)
            .map_err(|_| DecodeError::Invalid("inputs over capacity"))?;
    }
    let has_seq = d.bool()?;
    let seq = d.u32()?;
    s.last_seq = has_seq.then_some(InputSeq(seq));
    s.last_input = MoveInput::decode(d)?;
    let n = d.u32()?;
    for _ in 0..n {
        let p = vec3_of(d)?;
        let ms = d.u32()?;
        s.claims
            .push((p, ms))
            .map_err(|_| DecodeError::Invalid("claims over capacity"))?;
    }
    s.envelope = envelope_of(d)?;
    s.cheats = d.u32()?;
    s.synthesized = d.u32()?;
    s.synth_mask = d.u64()?;
    s.credits = d.u8()?;
    s.cooldown = d.u32()?;
    s.late = d.u32()?;
    s.pauses = d.u32()?;
    s.skipped = d.u32()?;
    Ok(s)
}

/// One ghost, exactly.
pub(crate) fn replicated(e: &mut Encoder<'_>, r: &Replicated) {
    e.i64(r.key);
    e.u64(r.repl.0.to_bits());
    vec3(e, r.position);
    vec3(e, r.velocity);
    e.u16(r.yaw.0);
    e.u32(r.look.0);
}

/// Reads one ghost written by [`replicated`].
pub(crate) fn replicated_of(d: &mut Decoder<'_>) -> Result<Replicated, DecodeError> {
    Ok(Replicated {
        key: d.i64()?,
        repl: ReplicationId(mantis_core::ecs::EntityId::from_bits(d.u64()?)),
        position: vec3_of(d)?,
        velocity: vec3_of(d)?,
        yaw: Angle16(d.u16()?),
        look: AppearanceId(d.u32()?),
    })
}

/// A snapshot could not be written or restored.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct SnapshotError(pub String);

impl core::fmt::Display for SnapshotError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "snapshot: {}", self.0)
    }
}

impl std::error::Error for SnapshotError {}

impl From<DecodeError> for SnapshotError {
    fn from(e: DecodeError) -> Self {
        Self(e.to_string())
    }
}
