//! Cell-side session state and the allowed-state dispatch list (plan 7.3).

use std::collections::BTreeMap;

use mantis_adapter_contract::core_types::{InputSeq, MoveInput, Tick, Vec3};
use mantis_adapter_contract::{ExtensionRefused, MovementMode, SetPosition};
use mantis_core::ecs::{EntityId, Resource};
use mantis_core::hash::{StableHasher, StateHash};
use mantis_core::log::SessionId;
use mantis_core::mem::BoundedVec;

use crate::components::ReplicationId;
use crate::intent::CellIntent;

/// The most inputs a Predictive session can buffer: the capacity behind
/// every cell's window ([`crate::movement::InputConfig::window`]).
pub const INPUT_WINDOW_MAX: usize = 64;
/// Claims a Validated session may have pending in one tick.
pub const CLAIM_WINDOW: usize = 8;

/// Validated-mode envelope bookkeeping (decision 0011).
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct EnvelopeState {
    /// The last accepted (or corrected-to) position.
    pub last_pos: Vec3,
    /// The client clock of the last accepted claim.
    pub last_client_ms: Option<u32>,
    /// The smallest observed `client_ms - server_ms` (best-case latency).
    pub min_offset_ms: Option<i64>,
    /// Server time the avatar entered the cell (bounds the first claim).
    pub joined_ms: i64,
    /// A correction was sent: claims are ignored until the client reports
    /// a position within reach of the corrected one.
    pub awaiting_correction: bool,
    /// Server time of the last correction.
    pub corrected_ms: i64,
}

impl EnvelopeState {
    /// A fresh envelope for an avatar placed at `at` at server time `now_ms`.
    #[must_use]
    pub const fn new(at: Vec3, now_ms: i64) -> Self {
        Self {
            last_pos: at,
            last_client_ms: None,
            min_offset_ms: None,
            joined_ms: now_ms,
            awaiting_correction: false,
            corrected_ms: now_ms,
        }
    }
}

/// One session's state inside its cell. Simulation state: hashed.
#[derive(Clone, Debug)]
pub struct CellSession {
    /// The session.
    pub id: SessionId,
    /// Its adapter's movement mode.
    pub mode: MovementMode,
    /// Its avatar in this cell.
    pub avatar: Option<EntityId>,
    /// The avatar's stable identity.
    pub repl: ReplicationId,
    /// The character's lease epoch in this cell.
    pub epoch: u64,
    /// The persistent character identity.
    pub character: u64,
    /// Predictive: buffered inputs, sorted by seq.
    pub inputs: BoundedVec<MoveInput>,
    /// Predictive: the last applied seq (`ack`).
    pub last_seq: Option<InputSeq>,
    /// Predictive: the last applied input (repeated for synthesized steps).
    pub last_input: MoveInput,
    /// Validated: claims received this tick, in order.
    pub claims: BoundedVec<(Vec3, u32)>,
    /// Validated: envelope state.
    pub envelope: EnvelopeState,
    /// Envelope violations and refused intents (readable by Ops).
    pub cheats: u32,
    /// Synthesized (repeated) steps, a lost-input diagnostic.
    pub synthesized: u32,
    /// Predictive: bit `i` set when seq `last_seq - i` was synthesized (a
    /// real input for it arriving later is late, not a duplicate).
    pub synth_mask: u64,
    /// Predictive: realigning pauses owed (one per late input, bounded by
    /// [`crate::movement::InputConfig::max_lead`]).
    pub credits: u8,
    /// Predictive: ticks until the next pause is allowed.
    pub cooldown: u32,
    /// Predictive: real inputs that arrived after their seq was synthesized.
    pub late: u32,
    /// Predictive: ticks consumption paused to realign (counted, bounded).
    pub pauses: u32,
    /// Predictive: seqs skipped unapplied because an input arrived beyond
    /// the window (the cell's clock slipped, or the client ran ahead).
    pub skipped: u32,
}

impl CellSession {
    /// A session entering the cell.
    #[must_use]
    pub fn new(
        id: SessionId,
        mode: MovementMode,
        repl: ReplicationId,
        epoch: u64,
        at: Vec3,
        now_ms: i64,
    ) -> Self {
        Self {
            id,
            mode,
            avatar: None,
            repl,
            epoch,
            character: 0,
            inputs: BoundedVec::with_capacity(INPUT_WINDOW_MAX),
            last_seq: None,
            last_input: MoveInput::default(),
            claims: BoundedVec::with_capacity(CLAIM_WINDOW),
            envelope: EnvelopeState::new(at, now_ms),
            cheats: 0,
            synthesized: 0,
            synth_mask: 0,
            credits: 0,
            cooldown: 0,
            late: 0,
            pauses: 0,
            skipped: 0,
        }
    }
}

impl StateHash for CellSession {
    fn state_hash(&self, h: &mut StableHasher) {
        h.write_u64(self.id.0);
        h.write_u8(self.mode as u8);
        self.avatar.state_hash(h);
        self.repl.state_hash(h);
        h.write_u64(self.epoch);
        h.write_u64(self.character);
        h.write_u64(self.inputs.len() as u64);
        for i in self.inputs.iter() {
            i.state_hash(h);
        }
        self.last_seq.map(|s| s.0).state_hash(h);
        self.last_input.state_hash(h);
        h.write_u64(self.claims.len() as u64);
        for (p, ms) in self.claims.iter() {
            p.state_hash(h);
            h.write_u32(*ms);
        }
        self.envelope.last_pos.state_hash(h);
        self.envelope.last_client_ms.state_hash(h);
        self.envelope.min_offset_ms.state_hash(h);
        h.write_u64(self.envelope.joined_ms.cast_unsigned());
        self.envelope.awaiting_correction.state_hash(h);
        h.write_u64(self.envelope.corrected_ms.cast_unsigned());
        h.write_u32(self.cheats);
        h.write_u32(self.synthesized);
        h.write_u64(self.synth_mask);
        h.write_u8(self.credits);
        h.write_u32(self.cooldown);
        h.write_u32(self.late);
        h.write_u32(self.pauses);
        h.write_u32(self.skipped);
    }
}

/// The sessions of a cell, keyed by id (iteration order is deterministic),
/// plus this tick's corrections for Validated clients.
#[derive(Debug)]
pub struct Sessions {
    /// By session id.
    pub map: BTreeMap<SessionId, CellSession>,
    /// Hard corrections produced this tick. Output, not state: drained every
    /// tick and excluded from the hash.
    pub corrections: BoundedVec<(SessionId, SetPosition)>,
    /// Extension refusals to answer this tick. Output, like corrections.
    pub refusals: BoundedVec<(SessionId, ExtensionRefused)>,
}

impl Sessions {
    /// No sessions; room for `corrections` corrections per tick.
    #[must_use]
    pub fn new(corrections: usize) -> Self {
        Self {
            map: BTreeMap::new(),
            corrections: BoundedVec::with_capacity(corrections),
            refusals: BoundedVec::with_capacity(corrections),
        }
    }
}

/// Hashes the sessions; corrections are output and excluded.
impl StateHash for Sessions {
    fn state_hash(&self, h: &mut StableHasher) {
        h.write_u64(self.map.len() as u64);
        for s in self.map.values() {
            s.state_hash(h);
        }
    }
}

impl Resource for Sessions {
    const NAME: &'static str = "server.sessions";

    fn save(&self, e: &mut mantis_adapter_contract::core_types::Encoder<'_>) -> mantis_core::ecs::Saved {
        e.u32(u32::try_from(self.map.len()).unwrap_or(u32::MAX));
        for s in self.map.values() {
            crate::snapshot::session(e, s);
        }
        mantis_core::ecs::Saved::Written
    }

    fn load(
        &mut self,
        d: &mut mantis_adapter_contract::core_types::Decoder<'_>,
    ) -> Result<(), mantis_adapter_contract::core_types::DecodeError> {
        self.map.clear();
        self.corrections.clear();
        self.refusals.clear();
        let n = d.u32()?;
        for _ in 0..n {
            let s = crate::snapshot::session_of(d)?;
            self.map.insert(s.id, s);
        }
        Ok(())
    }
}

/// Why an intent was refused at dispatch.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Refusal {
    /// The session is not in this cell (only `Join` and transfers are allowed).
    NotJoined,
    /// The session already has an avatar here.
    AlreadyJoined,
    /// The intent does not fit the session's movement mode.
    WrongMode,
    /// The session's buffer is full.
    Overflow,
    /// Only the host itself may send this (module switches).
    SystemOnly,
}

/// The allowed-state list: decides before any handler runs (plan 7.3).
///
/// # Errors
/// The [`Refusal`]; the caller counts it against the session.
pub fn allowed(session: Option<&CellSession>, intent: &CellIntent) -> Result<(), Refusal> {
    match (session, intent) {
        (
            None,
            CellIntent::Join { .. }
            | CellIntent::TransferIn(_)
            | CellIntent::TransferAck(_)
            | CellIntent::SetModule { .. }
            | CellIntent::ScriptReload { .. }
            | CellIntent::SetLive { .. }
            | CellIntent::ServiceUpdate { .. }
            | CellIntent::Relocate { .. }
            | CellIntent::SetModTier { .. }
            | CellIntent::ClockSlip { .. },
        )
        | (Some(_), CellIntent::Throttled { .. }) => Ok(()),
        (
            Some(_),
            CellIntent::SetModule { .. }
            | CellIntent::ScriptReload { .. }
            | CellIntent::SetLive { .. }
            | CellIntent::ServiceUpdate { .. }
            | CellIntent::Relocate { .. }
            | CellIntent::SetModTier { .. }
            | CellIntent::ClockSlip { .. },
        ) => Err(Refusal::SystemOnly),
        (None, _) => Err(Refusal::NotJoined),
        (Some(_), CellIntent::Join { .. } | CellIntent::TransferIn(_)) => Err(Refusal::AlreadyJoined),
        (Some(s), CellIntent::Move(_)) if s.mode != MovementMode::Predictive => Err(Refusal::WrongMode),
        (Some(s), CellIntent::MoveClaim { .. }) if s.mode != MovementMode::Validated => {
            Err(Refusal::WrongMode)
        }
        (Some(_), _) => Ok(()),
    }
}

/// The server tick in milliseconds since tick zero (integer arithmetic).
#[must_use]
pub fn tick_ms(tick: Tick, hz: u32) -> i64 {
    i64::try_from(u128::from(tick.0) * 1000 / u128::from(hz.max(1))).unwrap_or(i64::MAX)
}
