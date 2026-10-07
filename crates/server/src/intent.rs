//! What a cell's inbox carries, and what its log records.
//!
//! Every input that can change a cell's world arrives as a [`CellIntent`]
//! through the inbox and is appended to the cell's unified log when the
//! `Inbound` phase takes it (plan 6.8). That includes ownership transfers
//! between cells, so a cell replays exactly from its own log. Snapshot
//! acknowledgements affect only replication state, never the world, and do
//! not pass through here.

use mantis_adapter_contract::core_types::{
    DecodeError, Decoder, Encoder, EntityId, InputSeq, MotionModifiers, MotionState, MoveInput, Vec3, Wire,
};
use mantis_adapter_contract::{AppearanceId, Cast, Choose, Interact, MovementMode};
use mantis_core::kinematics::Angle16;

use crate::components::ReplicationId;
use crate::modules::{ModuleCommand, ModuleOutcome, Payload};
use crate::session::EnvelopeState;

/// A transferred entity's state (plan 7.1 ownership transfer).
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Transfer {
    /// The stable identity.
    pub repl: ReplicationId,
    /// Kinematic state at hand-off.
    pub body: MotionState,
    /// Modifiers at hand-off.
    pub mods: MotionModifiers,
    /// Appearance.
    pub look: AppearanceId,
    /// The controlling session, for avatars.
    pub session: Option<u64>,
    /// The session's movement mode.
    pub mode: MovementMode,
    /// The last applied input seq (Predictive avatars).
    pub last_seq: Option<InputSeq>,
    /// The lease epoch granted to the destination.
    pub epoch: u64,
    /// The session's Validated envelope and cheat count: they travel with
    /// the avatar, so a border crossing never resets the movement check.
    pub envelope: EnvelopeState,
    /// The session's cheat counter.
    pub cheats: u32,
    /// The session's character identity.
    pub character: u64,
    /// The session's Predictive input state.
    pub input: crate::movement::InputCarry,
}

/// Optional fields on the wire (logs and snapshots): a presence flag, then
/// the value, 0 when absent.
pub(crate) fn put_opt_u32(e: &mut Encoder<'_>, v: Option<u32>) {
    e.bool(v.is_some());
    e.u32(v.unwrap_or(0));
}

/// A tag and one `u32`: the encoding of the one-number intents.
fn tag_u32(e: &mut Encoder<'_>, tag: u8, v: u32) {
    e.u8(tag);
    e.u32(v);
}

pub(crate) fn put_opt_i64(e: &mut Encoder<'_>, v: Option<i64>) {
    e.bool(v.is_some());
    e.i64(v.unwrap_or(0));
}

pub(crate) fn get_opt_u32(d: &mut Decoder<'_>) -> Result<Option<u32>, DecodeError> {
    let some = d.bool()?;
    let v = d.u32()?;
    Ok(some.then_some(v))
}

pub(crate) fn get_opt_i64(d: &mut Decoder<'_>) -> Result<Option<i64>, DecodeError> {
    let some = d.bool()?;
    let v = d.i64()?;
    Ok(some.then_some(v))
}

/// The version of the cell log's record encodings (intents, commands,
/// outcomes, transfers). A package folds it into its build id, so a log
/// written by a binary with other encodings is refused by its header, never
/// misread. Bump it with any encoding change; the fingerprint test fails
/// until you do.
pub const LOG_SCHEMA_VERSION: u32 = 3;

/// One inbox item.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum CellIntent {
    /// The session's avatar enters this cell.
    Join {
        /// Identity to replicate under.
        repl: ReplicationId,
        /// Where it spawns.
        spawn: Vec3,
        /// Facing.
        yaw: Angle16,
        /// Appearance.
        look: AppearanceId,
        /// Movement mode of the session's adapter.
        mode: MovementMode,
        /// The character's lease epoch.
        epoch: u64,
        /// The persistent character identity (from the account service; the
        /// key modules store per-character data under).
        character: u64,
    },
    /// The session leaves (its avatar despawns).
    Leave,
    /// One tick of input (Predictive).
    Move(MoveInput),
    /// A reported position (Validated).
    MoveClaim {
        /// The claimed position.
        position: Vec3,
        /// The client clock, ms.
        client_time_ms: u32,
    },
    /// Use an ability.
    Cast(Cast),
    /// Interact.
    Interact(Interact),
    /// Answer a prompt.
    Choose(Choose),
    /// An entity arrives from a neighbouring cell.
    TransferIn(Transfer),
    /// A neighbour accepted an entity this cell offered: despawn it here.
    TransferAck(ReplicationId),
    /// A package-defined extension intent, for the module that registered
    /// its kind.
    Extension {
        /// The extension kind.
        kind: mantis_adapter_contract::ExtensionKind,
        /// The client's request id, echoed in a refusal (0: untracked).
        request: u32,
        /// Its payload.
        payload: Payload,
    },
    /// Enables or disables a module (a feature flag from Ops or start-up).
    /// Has no session; the host never maps a client message to it.
    SetModule {
        /// Index of the module in the cell's set.
        module: u16,
        /// Enabled.
        enabled: bool,
    },
    /// A development hot reload of a server script (lead ruling, M5): the
    /// log records which source by hash, and replay refuses to cross it
    /// unless the replayer supplies the same source.
    ScriptReload {
        /// The script's name.
        name: mantis_adapter_contract::core_types::WireString<64>,
        /// BLAKE3 of the new source.
        source: mantis_adapter_contract::core_types::ContentHash,
    },
    /// A live flag or tunable change from Ops, verified by the host before
    /// it was queued, applied at the start of the next tick, and logged so
    /// replay applies it at the same tick.
    SetLive {
        /// A module key, `<module key>.<flag>`, or a tunable's full name.
        name: mantis_adapter_contract::core_types::WireString<96>,
        /// [`crate::modules::LIVE_FLAG`] or [`crate::modules::LIVE_TUNABLE`].
        kind: u8,
        /// The value (flags: 0 or 1).
        value: f32,
    },
    /// An update from a service role (a line social delivered here),
    /// logged like a client intent and dispatched to the module that
    /// registered its topic (lead ruling, M7).
    ServiceUpdate {
        /// The topic ([`crate::service`]).
        topic: u16,
        /// Its payload.
        payload: Payload,
    },
    /// Sets the tier client modules run at in this cell (an instance's
    /// policy: a competitive instance permits presentation only). Every
    /// session is sent its permitted list again. System-only, logged.
    SetModTier {
        /// The tier.
        tier: mantis_adapter_contract::ModTier,
    },
    /// The cell's clock slipped behind wall time: its host stalled or
    /// overran by `ticks` ticks and re-anchored without catching up
    /// (docs/SERVER.md section 2). Validated clocks are judged against tick
    /// time, so each session's clock baseline moves by the slip; nothing
    /// else changes. System-only, logged.
    ClockSlip {
        /// Ticks of wall time the cell did not tick.
        ticks: u32,
    },
    /// The host refused `count` of this session's messages for its rate
    /// limits: added to the session's cheat counter. Only the host creates
    /// it; no client message maps to it.
    Throttled {
        /// Messages refused.
        count: u32,
    },
    /// A gateway told the host its client's connection dropped (`up`
    /// false) or came back with a resume ticket (`up` true), or the host
    /// handed the session to another host. Down: buffered inputs are
    /// dropped and the avatar repeats an idle input (no button held), so it
    /// comes to rest. Up: the input stream starts over, as for a new
    /// session: the next `Move` defines the seq, with no wait for the seqs
    /// lost with the old connection (snapshots carry no `ack` until then).
    /// Predictive sessions only; only the host creates it.
    Linked {
        /// The client is connected.
        up: bool,
    },
    /// Moves a character's avatar to `position` (matchmaking placing a
    /// group in an instance, or bringing it back). The client is corrected
    /// to the new position; when it lies in another cell's range the zone
    /// transfers the avatar there after the tick. System-only, logged.
    Relocate {
        /// The character.
        character: u64,
        /// Where to.
        position: Vec3,
    },
}

fn put_mode(e: &mut Encoder<'_>, m: MovementMode) {
    m.encode(e);
}

impl Wire for Transfer {
    fn encode(&self, e: &mut Encoder<'_>) {
        self.repl.0.encode(e);
        self.body.position.encode(e);
        self.body.velocity.encode(e);
        self.body.yaw.encode(e);
        e.bool(self.body.grounded);
        e.f32(self.mods.speed_scale);
        e.f32(self.mods.jump_scale);
        e.f32(self.mods.gravity_scale);
        self.look.encode(e);
        self.session.encode(e);
        put_mode(e, self.mode);
        self.last_seq.encode(e);
        e.u64(self.epoch);
        let env = &self.envelope;
        env.last_pos.encode(e);
        put_opt_u32(e, env.last_client_ms);
        put_opt_i64(e, env.min_offset_ms);
        e.i64(env.joined_ms);
        e.bool(env.awaiting_correction);
        e.i64(env.corrected_ms);
        e.u32(self.cheats);
        e.u64(self.character);
        self.input.encode(e);
    }
    fn decode(d: &mut Decoder<'_>) -> Result<Self, DecodeError> {
        Ok(Self {
            repl: ReplicationId(EntityId::decode(d)?),
            body: MotionState {
                position: Vec3::decode(d)?,
                velocity: Vec3::decode(d)?,
                yaw: Angle16::decode(d)?,
                grounded: d.bool()?,
            },
            mods: MotionModifiers {
                speed_scale: d.finite_f32()?,
                jump_scale: d.finite_f32()?,
                gravity_scale: d.finite_f32()?,
            },
            look: AppearanceId::decode(d)?,
            session: Option::<u64>::decode(d)?,
            mode: MovementMode::decode(d)?,
            last_seq: Option::<InputSeq>::decode(d)?,
            epoch: d.u64()?,
            envelope: EnvelopeState {
                last_pos: Vec3::decode(d)?,
                last_client_ms: get_opt_u32(d)?,
                min_offset_ms: get_opt_i64(d)?,
                joined_ms: d.i64()?,
                awaiting_correction: d.bool()?,
                corrected_ms: d.i64()?,
            },
            cheats: d.u32()?,
            character: d.u64()?,
            input: crate::movement::InputCarry::decode(d)?,
        })
    }
}

/// Tagged encoding: one byte tag, then the variant's fields.
impl Wire for CellIntent {
    fn encode(&self, e: &mut Encoder<'_>) {
        match self {
            Self::Join {
                repl,
                spawn,
                yaw,
                look,
                mode,
                epoch,
                character,
            } => {
                e.u8(1);
                repl.0.encode(e);
                spawn.encode(e);
                yaw.encode(e);
                look.encode(e);
                put_mode(e, *mode);
                e.u64(*epoch);
                e.u64(*character);
            }
            Self::Leave => e.u8(2),
            Self::Move(m) => {
                e.u8(3);
                m.encode(e);
            }
            Self::MoveClaim {
                position,
                client_time_ms,
            } => {
                e.u8(4);
                position.encode(e);
                e.u32(*client_time_ms);
            }
            Self::Cast(c) => {
                e.u8(5);
                c.encode(e);
            }
            Self::Interact(i) => {
                e.u8(6);
                i.encode(e);
            }
            Self::Choose(c) => {
                e.u8(7);
                c.encode(e);
            }
            Self::TransferIn(t) => {
                e.u8(8);
                t.encode(e);
            }
            Self::TransferAck(r) => {
                e.u8(9);
                r.0.encode(e);
            }
            Self::Extension {
                kind,
                request,
                payload,
            } => {
                e.u8(10);
                e.u16(kind.0);
                e.u32(*request);
                payload.encode(e);
            }
            Self::SetModule { module, enabled } => {
                e.u8(11);
                e.u16(*module);
                e.bool(*enabled);
            }
            Self::ScriptReload { name, source } => {
                e.u8(12);
                name.encode(e);
                source.encode(e);
            }
            Self::SetLive { name, kind, value } => {
                e.u8(13);
                name.encode(e);
                e.u8(*kind);
                e.f32(*value);
            }
            Self::ServiceUpdate { topic, payload } => {
                e.u8(14);
                e.u16(*topic);
                payload.encode(e);
            }
            Self::Throttled { count } => tag_u32(e, 16, *count),
            Self::SetModTier { tier } => {
                e.u8(17);
                tier.encode(e);
            }
            Self::ClockSlip { ticks } => tag_u32(e, 18, *ticks),
            Self::Linked { up } => {
                e.u8(19);
                e.bool(*up);
            }
            Self::Relocate { character, position } => {
                e.u8(15);
                e.u64(*character);
                position.encode(e);
            }
        }
    }

    fn decode(d: &mut Decoder<'_>) -> Result<Self, DecodeError> {
        Ok(match d.u8()? {
            1 => Self::Join {
                repl: ReplicationId(EntityId::decode(d)?),
                spawn: Vec3::decode(d)?,
                yaw: Angle16::decode(d)?,
                look: AppearanceId::decode(d)?,
                mode: MovementMode::decode(d)?,
                epoch: d.u64()?,
                character: d.u64()?,
            },
            2 => Self::Leave,
            3 => Self::Move(MoveInput::decode(d)?),
            4 => Self::MoveClaim {
                position: Vec3::decode(d)?,
                client_time_ms: d.u32()?,
            },
            5 => Self::Cast(Cast::decode(d)?),
            6 => Self::Interact(Interact::decode(d)?),
            7 => Self::Choose(Choose::decode(d)?),
            8 => Self::TransferIn(Transfer::decode(d)?),
            9 => Self::TransferAck(ReplicationId(EntityId::decode(d)?)),
            10 => Self::Extension {
                kind: mantis_adapter_contract::ExtensionKind(d.u16()?),
                request: d.u32()?,
                payload: Payload::decode(d)?,
            },
            11 => Self::SetModule {
                module: d.u16()?,
                enabled: d.bool()?,
            },
            12 => Self::ScriptReload {
                name: Wire::decode(d)?,
                source: Wire::decode(d)?,
            },
            13 => Self::SetLive {
                name: Wire::decode(d)?,
                kind: d.u8()?,
                // Raw bits: a non-finite value is refused when applied, never
                // when the log is read, so a log stays replayable.
                value: f32::from_bits(d.u32()?),
            },
            14 => Self::ServiceUpdate {
                topic: d.u16()?,
                payload: Payload::decode(d)?,
            },
            16 => Self::Throttled { count: d.u32()? },
            17 => Self::SetModTier {
                tier: mantis_adapter_contract::ModTier::decode(d)?,
            },
            18 => Self::ClockSlip { ticks: d.u32()? },
            19 => Self::Linked { up: d.bool()? },
            15 => Self::Relocate {
                character: d.u64()?,
                position: Vec3::decode(d)?,
            },
            _ => return Err(DecodeError::Invalid("cell intent tag")),
        })
    }
}

/// The cell log schema.
#[derive(Debug)]
pub struct CellLogSchema;

impl mantis_core::log::LogSchema for CellLogSchema {
    type Intent = CellIntent;
    type Command = ModuleCommand;
    type Outcome = ModuleOutcome;
}

#[cfg(test)]
mod tests {
    use super::*;
    use mantis_adapter_contract::AbilityId;
    use mantis_adapter_contract::core_types::{Tick, decode_exact, encode_into};

    #[test]
    fn every_variant_round_trips() {
        let t = Transfer {
            repl: ReplicationId(EntityId::new(4, 0)),
            body: MotionState::at_rest(Vec3::new(1.0, 2.0, 3.0), Angle16(9)),
            mods: MotionModifiers::NONE,
            look: AppearanceId(2),
            session: Some(7),
            mode: MovementMode::Predictive,
            last_seq: Some(InputSeq(12)),
            epoch: 3,
            envelope: EnvelopeState {
                last_pos: Vec3::new(1.0, 2.0, 3.0),
                last_client_ms: Some(4_000),
                min_offset_ms: Some(-120),
                joined_ms: 33,
                awaiting_correction: true,
                corrected_ms: 900,
            },
            cheats: 2,
            character: 77,
            input: crate::movement::InputCarry {
                synthesized: 5,
                synth_mask: 0b1011,
                credits: 2,
                cooldown: 3,
                late: 4,
                pauses: 1,
                ..crate::movement::InputCarry::default()
            },
        };
        let all = [
            CellIntent::ClockSlip { ticks: 15 },
            CellIntent::Linked { up: false },
            CellIntent::Linked { up: true },
            CellIntent::Join {
                repl: ReplicationId(EntityId::new(1, 0)),
                spawn: Vec3::X,
                yaw: Angle16(5),
                look: AppearanceId(1),
                mode: MovementMode::Validated,
                epoch: 1,
                character: 9,
            },
            CellIntent::Leave,
            CellIntent::Move(MoveInput::default()),
            CellIntent::MoveClaim {
                position: Vec3::Z,
                client_time_ms: 99,
            },
            CellIntent::Cast(Cast {
                ability: AbilityId(1),
                target: None,
                view_tick: Tick(3),
                view_frac: 0,
            }),
            CellIntent::TransferIn(t),
            CellIntent::TransferAck(ReplicationId(EntityId::new(4, 0))),
        ];
        let mut every = Vec::new();
        for i in all {
            let mut b = Vec::new();
            encode_into(&i, &mut b);
            assert_eq!(decode_exact::<CellIntent>(&b), Ok(i));
            every.extend_from_slice(&b);
        }
        assert!(decode_exact::<CellIntent>(&[0]).is_err());
        // The encodings' fingerprint: an encoding change moves it, and must
        // bump LOG_SCHEMA_VERSION (folded into a package's build id, so old
        // logs are refused by their header instead of misread).
        let fingerprint = mantis_core::hash::StableHasher::hash_bytes(&every);
        assert_eq!(
            (LOG_SCHEMA_VERSION, fingerprint),
            (LOG_SCHEMA_VERSION_OF_FINGERPRINT, LOG_FINGERPRINT),
            "the cell log encodings changed: bump LOG_SCHEMA_VERSION, then record              LOG_SCHEMA_VERSION_OF_FINGERPRINT = it and LOG_FINGERPRINT = {fingerprint:#018x}"
        );
    }

    /// The schema version the fingerprint below was recorded at.
    const LOG_SCHEMA_VERSION_OF_FINGERPRINT: u32 = 3;
    /// The intents' encodings at that version.
    const LOG_FINGERPRINT: u64 = 0x7228_4b09_6017_98c3;
}
